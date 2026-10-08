#!/usr/bin/env python3
"""Canonicalize a mneme-bootstrap draft and emit a hash-complete plan."""

from __future__ import annotations

import argparse
import json
from pathlib import Path
import sys

from inventory_repo import (
    InventoryError,
    load_inventory_artifact,
    pretty_json_bytes,
    publish_json_no_clobber,
    resolve_sidecar_path,
)
from plan_contract import (
    BOOTSTRAP_EDGE_KINDS,
    FULL_EDGE_ACTIONS,
    FULL_NODE_ACTIONS,
    MODES,
    NAMESPACE,
    POLICY_VERSION,
    SCHEMA_VERSION,
    canonical_bytes,
    canonical_sources,
    canonical_unit_number,
    edge_assertion_hash,
    expected_post_manifest_delta,
    materialization_hash,
    node_content_hash,
    normalize_text,
    plan_hash,
    render_body,
)
from secret_scan import summary as secret_summary


class BuildError(RuntimeError):
    pass


def canonicalize(draft: object, inventory: dict) -> dict:
    if not isinstance(draft, dict):
        raise BuildError("draft root must be an object")
    if "sources" in draft or "exclusions" in draft:
        raise BuildError(
            "draft sources and exclusions are builder-owned; use source_decisions "
            "and the bound inventory artifact"
        )
    plan = dict(draft)
    plan["schema_version"] = SCHEMA_VERSION
    plan["namespace"] = NAMESPACE
    plan["policy_version"] = POLICY_VERSION
    plan.pop("plan_hash", None)
    plan.pop("post_manifest_delta", None)

    binding = plan.get("inventory")
    if not isinstance(binding, dict) or set(binding) != {"path", "sha256"}:
        raise BuildError("draft.inventory must bind an exact path and sha256")
    if binding.get("sha256") != inventory["inventory_hash"]:
        raise BuildError("draft inventory sha256 disagrees with the artifact")

    expected_repo = {
        "head": inventory["repo"]["head"],
        "tree": inventory["repo"]["tree"],
        "object_format": inventory["repo"]["object_format"],
        "dirty_digest": None,
    }
    if plan.get("repo") != expected_repo:
        raise BuildError("draft.repo must exactly match the bound inventory source cut")
    expected_base = {
        "manifest_path": inventory["manifest"]["path"],
        "manifest_generation": inventory["manifest"]["generation"] or 0,
        "manifest_hash": inventory["manifest"]["sha256"],
    }
    if plan.get("base") != expected_base:
        raise BuildError("draft.base must exactly match the bound inventory manifest cut")

    mode = plan.get("mode")
    if mode not in MODES:
        raise BuildError(f"draft.mode must be one of {sorted(MODES)!r}")
    repo = plan.get("repo")
    if not isinstance(repo, dict) or not isinstance(repo.get("head"), str):
        raise BuildError("draft.repo.head is required")

    decisions = plan.pop("source_decisions", None)
    inventory_files = inventory.get("files")
    if not isinstance(decisions, dict) or not isinstance(inventory_files, list):
        raise BuildError("draft.source_decisions must classify every selected inventory path")
    selected_paths = {item["path"] for item in inventory_files}
    if set(decisions) != selected_paths:
        missing = sorted(selected_paths - set(decisions))
        extra = sorted(set(decisions) - selected_paths)
        raise BuildError(
            f"source_decisions must exactly cover selected inventory paths; "
            f"missing={missing!r}, extra={extra!r}"
        )
    if any(value not in {"inspect", "evidence", "deferred"} for value in decisions.values()):
        raise BuildError("source_decisions values must be inspect, evidence, or deferred")
    plan["sources"] = sorted(
        (
            {
                "path": item["path"],
                "blob": item["blob"],
                "sha256": item["sha256"],
                "bytes": item["bytes"],
                "class": item["class"],
                "decision": decisions[item["path"]],
            }
            for item in inventory_files
        ),
        key=lambda source: source["path"],
    )

    raw_nodes = plan.get("nodes", [])
    if not isinstance(raw_nodes, list):
        raise BuildError("draft.nodes must be an array")
    nodes = []
    for raw in raw_nodes:
        if not isinstance(raw, dict):
            raise BuildError("every node must be an object")
        node = dict(raw)
        action = node.get("action")
        if action == "retirement_proposal":
            if {"claim", "body", "content_hash", "materialization_hash"} & set(node):
                raise BuildError(
                    f"retirement proposal {node.get('key')!r} cannot supply replacement content"
                )
            if isinstance(node.get("reason"), str):
                node["reason"] = normalize_text(node["reason"])
            deleted = node.get("deleted_source_paths", [])
            if not isinstance(deleted, list):
                raise BuildError(
                    f"retirement proposal {node.get('key')!r} requires deleted_source_paths"
                )
            node["deleted_source_paths"] = sorted(set(deleted))
            nodes.append(node)
            continue
        if action == "noop":
            if {"claim", "body", "content_hash", "materialization_hash"} & set(node):
                raise BuildError(f"noop {node.get('key')!r} is expectation-only")
            node["sources"] = canonical_sources(node.get("sources", []))
            nodes.append(node)
            continue
        if action not in FULL_NODE_ACTIONS:
            raise BuildError(f"node {node.get('key')!r} has unsupported action {action!r}")
        if {"body", "content_hash", "materialization_hash"} & set(node):
            raise BuildError("draft nodes use claim; the builder owns bodies and hashes")
        claim = node.pop("claim", None)
        if not isinstance(claim, str):
            raise BuildError(f"node {node.get('key')!r} requires a string claim")
        if not isinstance(node.get("summary"), str):
            raise BuildError(f"node {node.get('key')!r} requires a string summary")
        node["summary"] = normalize_text(node["summary"])
        claim = normalize_text(claim)
        if not node["summary"] or not claim:
            raise BuildError(f"node {node.get('key')!r} requires nonblank summary and claim")
        node["sources"] = canonical_sources(node.get("sources", []))
        node["tags"] = sorted(set(node.get("tags", [])))
        node["stability"] = canonical_unit_number(
            node.get("stability"), f"node {node.get('key')!r} stability"
        )
        node["confidence"] = canonical_unit_number(
            node.get("confidence"), f"node {node.get('key')!r} confidence"
        )
        content_hash = node_content_hash(
            key=node["key"],
            summary=node["summary"],
            claim=claim,
            sources=node["sources"],
        )
        body = render_body(
            key=node["key"],
            content_hash=content_hash,
            source_commit=repo["head"],
            claim=claim,
        )
        node["content_hash"] = content_hash
        node["body"] = body
        node["materialization_hash"] = materialization_hash(
            content_hash=content_hash,
            body=body,
            tags=node["tags"],
            status=node["status"],
            stability=node["stability"],
            confidence=node["confidence"],
        )
        nodes.append(node)
    plan["nodes"] = sorted(nodes, key=lambda node: node["key"])

    raw_edges = plan.get("edges", [])
    if not isinstance(raw_edges, list):
        raise BuildError("draft.edges must be an array")
    edges = []
    for raw in raw_edges:
        if not isinstance(raw, dict):
            raise BuildError("every edge must be an object")
        edge = dict(raw)
        action = edge.get("action")
        if action == "retirement_proposal":
            if {"sources", "weight", "assertion_hash"} & set(edge):
                raise BuildError(
                    f"edge retirement proposal {edge.get('key')!r} cannot supply replacement fields"
                )
            if isinstance(edge.get("reason"), str):
                edge["reason"] = normalize_text(edge["reason"])
            deleted = edge.get("deleted_source_paths", [])
            if not isinstance(deleted, list):
                raise BuildError(
                    f"edge retirement proposal {edge.get('key')!r} requires deleted_source_paths"
                )
            edge["deleted_source_paths"] = sorted(set(deleted))
            edges.append(edge)
            continue
        if action not in FULL_EDGE_ACTIONS | {"noop"}:
            raise BuildError(f"edge {edge.get('key')!r} has unsupported action {action!r}")
        if edge.get("kind") not in BOOTSTRAP_EDGE_KINDS:
            raise BuildError(
                f"edge {edge.get('key')!r} has unsupported assertion kind {edge.get('kind')!r}"
            )
        if edge.get("from_key") == edge.get("to_key"):
            raise BuildError(f"edge {edge.get('key')!r} cannot be a self-loop")
        if "assertion_hash" in edge:
            raise BuildError("the builder owns edge assertion hashes")
        edge["sources"] = canonical_sources(edge.get("sources", []))
        edge["weight"] = canonical_unit_number(
            edge.get("weight"), f"edge {edge.get('key')!r} weight"
        )
        edge["assertion_hash"] = edge_assertion_hash(
            key=edge["key"],
            from_key=edge["from_key"],
            to_key=edge["to_key"],
            kind=edge["kind"],
            weight=edge["weight"],
            sources=edge["sources"],
        )
        edges.append(edge)
    plan["edges"] = sorted(edges, key=lambda edge: edge["key"])

    dispositions = [dict(item) for item in plan.get("brownfield_dispositions", [])]
    for disposition in dispositions:
        if isinstance(disposition.get("reason"), str):
            disposition["reason"] = normalize_text(disposition["reason"])
    plan["brownfield_dispositions"] = sorted(
        dispositions, key=lambda item: item["legacy_node_id"]
    )
    findings = [dict(item) for item in plan.get("adversarial_findings", [])]
    for finding in findings:
        finding["sources"] = canonical_sources(finding.get("sources", []))
        for field in ("detail", "disposition"):
            if isinstance(finding.get(field), str):
                finding[field] = normalize_text(finding[field])
    plan["adversarial_findings"] = sorted(
        findings, key=lambda item: (item["kind"], item["detail"])
    )
    plan["exclusions"] = {
        "count": len(inventory["exclusions"]),
        "sha256": inventory["exclusions_sha256"],
    }
    plan["post_manifest_delta"] = expected_post_manifest_delta(plan)
    plan["plan_hash"] = plan_hash(plan)
    return plan


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("draft", type=Path)
    parser.add_argument("--root", default=".")
    parser.add_argument("--output")
    args = parser.parse_args()
    root = Path(args.root).resolve()
    try:
        raw = args.draft.read_bytes()
        if len(raw) > 2_097_152:
            raise BuildError("draft exceeds the hard 2 MiB contract limit")
        draft = json.loads(raw.decode("utf-8"))
        if not isinstance(draft, dict):
            raise BuildError("draft root must be an object")
        binding = draft.get("inventory")
        if not isinstance(binding, dict) or set(binding) != {"path", "sha256"}:
            raise BuildError("draft.inventory must bind an exact path and sha256")
        inventory_path, inventory_relative = resolve_sidecar_path(
            root, binding.get("path"), "draft.inventory.path"
        )
        if binding["path"] != inventory_relative:
            raise BuildError("draft.inventory.path must be canonical and repository-relative")
        inventory = load_inventory_artifact(inventory_path)
        plan = canonicalize(draft, inventory)
        detected_secret = secret_summary(canonical_bytes(plan))
        if detected_secret is not None:
            raise BuildError(
                f"plan contains high-confidence secret material ({detected_secret})"
            )
    except BuildError:
        raise
    except InventoryError as error:
        raise BuildError(str(error)) from error
    except (OSError, UnicodeError, json.JSONDecodeError, KeyError, TypeError, ValueError) as error:
        raise BuildError(f"cannot build plan: {error}") from error
    if args.output is not None:
        try:
            output_path, _output_relative = resolve_sidecar_path(
                root, args.output, "--output"
            )
        except InventoryError as error:
            raise BuildError(str(error)) from error
        protected_paths = {
            inventory_path,
            (root / plan["base"]["manifest_path"]).resolve(strict=False),
        }
        if output_path in protected_paths:
            raise BuildError("--output must not overwrite an input or manifest sidecar")
        try:
            publish_json_no_clobber(output_path, plan, "plan sidecar")
        except InventoryError as error:
            raise BuildError(str(error)) from error
    else:
        sys.stdout.buffer.write(pretty_json_bytes(plan))
    return 0


if __name__ == "__main__":
    try:
        raise SystemExit(main())
    except BuildError as error:
        print(f"build_plan.py: {error}", file=sys.stderr)
        raise SystemExit(2) from error
