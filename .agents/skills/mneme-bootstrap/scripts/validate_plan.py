#!/usr/bin/env python3
"""Validate a bounded mneme-bootstrap plan without mutating the database."""

from __future__ import annotations

import argparse
from collections import Counter
import hashlib
import json
import os
from pathlib import Path, PurePosixPath
import re
import subprocess
import sys

from inventory_repo import (
    DEFAULT_MAX_BYTES,
    InventoryError,
    classify,
    construct_inventory,
    content_exclusion_reason,
    exclusion_reason,
    ignored_paths,
    load_inventory_artifact,
    parse_manifest,
    resolve_sidecar_path,
    tree_records,
)
from manifest_contract import ManifestContractError, resolve_manifest_authority
from plan_contract import (
    APPLY_BLOCKING_FINDINGS,
    BOOTSTRAP_EDGE_KINDS,
    EXTRACTOR_VERSION,
    HARD_MAXIMA,
    MODES,
    NAMESPACE,
    POLICY_VERSION,
    SCHEMA_VERSION,
    canonical_bytes,
    canonical_sha256,
    canonical_sources,
    canonical_unit_number,
    edge_assertion_hash,
    expected_post_manifest_delta,
    has_forbidden_controls,
    materialization_hash,
    node_content_hash,
    normalize_text,
    parse_body,
    plan_hash,
)
from secret_scan import summary as secret_summary


STATUSES = {"active", "candidate"}
DISPOSITIONS = {"leave", "associate_proposal", "supersession_proposal", "adoption_proposal"}
FINDING_KINDS = {"conflict", "injection", "sensitive", "stale", "ambiguity", "ownership", "other"}
FINDING_STATUSES = {"resolved", "unresolved"}
HEX40_OR_64 = re.compile(r"^(?:[0-9a-f]{40}|[0-9a-f]{64})$")
HEX64 = re.compile(r"^[0-9a-f]{64}$")
ULID = re.compile(r"^[0-9A-HJKMNP-TV-Z]{26}$")
KEY = re.compile(r"^[a-z0-9][a-z0-9._:/#-]{0,255}$")
EDGE_KEY = re.compile(r"^[a-z0-9][a-z0-9._:/#>-]{0,511}$")
SPAN = re.compile(r"^[1-9][0-9]*:[1-9][0-9]*$")


class PlanError(RuntimeError):
    pass


def git(root: Path, *args: str) -> str:
    env = dict(os.environ)
    env["GIT_NO_REPLACE_OBJECTS"] = "1"
    env["GIT_GRAFT_FILE"] = os.devnull
    result = subprocess.run(
        ["git", "--no-replace-objects", "-C", str(root), *args],
        stdout=subprocess.PIPE,
        stderr=subprocess.PIPE,
        check=False,
        text=True,
        env=env,
    )
    if result.returncode != 0:
        raise PlanError(f"git {' '.join(args)} failed: {result.stderr.strip()}")
    return result.stdout.strip()


def require(condition: bool, message: str) -> None:
    if not condition:
        raise PlanError(message)


def safe_relative_path(value: object, context: str) -> str:
    require(isinstance(value, str) and bool(value), f"{context}: path is required")
    require(
        not any(
            ord(character) < 0x20 or 0x7F <= ord(character) <= 0x9F
            for character in value
        ),
        f"{context}: path contains a control character",
    )
    try:
        value.encode("utf-8", "strict")
    except UnicodeEncodeError as error:
        raise PlanError(f"{context}: path is not valid UTF-8") from error
    parsed = PurePosixPath(value)
    require(not parsed.is_absolute() and ".." not in parsed.parts, f"{context}: unsafe path")
    require(parsed.as_posix() == value and value != ".", f"{context}: path is noncanonical")
    return value


def is_agent_instruction_path(value: str) -> bool:
    parts = PurePosixPath(value).parts
    lower_parts = tuple(part.lower() for part in parts)
    name = lower_parts[-1]
    if name in {"agents.md", "claude.md", "gemini.md", "copilot-instructions.md"}:
        return True
    if name.endswith(("-instructions.md", ".instructions.md", "-prompt.md", ".prompt.md")):
        return True
    return any(part in {".agents", ".claude", ".cursor"} for part in lower_parts)


def utf8_bytes(value: str, context: str) -> bytes:
    try:
        return value.encode("utf-8", "strict")
    except UnicodeEncodeError as error:
        raise PlanError(f"{context}: text is not valid UTF-8") from error


def read_blob(root: Path, oid: str) -> bytes:
    result = subprocess.run(
        ["git", "--no-replace-objects", "-C", str(root), "cat-file", "blob", oid],
        stdout=subprocess.PIPE,
        stderr=subprocess.PIPE,
        check=False,
        env={
            **os.environ,
            "GIT_NO_REPLACE_OBJECTS": "1",
            "GIT_GRAFT_FILE": os.devnull,
        },
    )
    if result.returncode != 0:
        detail = result.stderr.decode("utf-8", "replace").strip()
        raise PlanError(f"cannot read source blob {oid}: {detail}")
    return result.stdout


def unit_number(value: object, context: str) -> float:
    try:
        number = canonical_unit_number(value, context)
    except ValueError as error:
        raise PlanError(str(error)) from error
    require(
        isinstance(value, float) and repr(value) != "-0.0",
        f"{context}: number is not in canonical float form",
    )
    return number


def validate_source(source: object, known_sources: dict[str, dict], context: str) -> None:
    require(isinstance(source, dict), f"{context}: source must be an object")
    require(
        set(source) == {"path", "blob", "sha256", "span"},
        f"{context}: source fields must be exact and include an inclusive span",
    )
    path = safe_relative_path(source.get("path"), context)
    blob = source.get("blob")
    sha256 = source.get("sha256")
    span = source.get("span")
    require(isinstance(blob, str) and HEX40_OR_64.fullmatch(blob) is not None, f"{context}: invalid blob")
    require(isinstance(sha256, str) and HEX64.fullmatch(sha256) is not None, f"{context}: invalid sha256")
    require(isinstance(span, str) and SPAN.fullmatch(span) is not None, f"{context}: inclusive span is required")
    require(path in known_sources, f"{context}: source path {path!r} is absent from plan inventory")
    known = known_sources[path]
    require(known["blob"] == blob and known["sha256"] == sha256, f"{context}: source digest disagrees with inventory")
    require(known["decision"] == "evidence", f"{context}: cited source is not marked as evidence")
    start, end = map(int, span.split(":"))
    require(start <= end, f"{context}: source span is reversed")
    require(end <= known["line_count"], f"{context}: source span exceeds blob line count")


def validate_source_list(
    value: object,
    sources: dict[str, dict],
    context: str,
    *,
    required: bool,
) -> list[dict]:
    require(isinstance(value, list), f"{context}: sources must be an array")
    require(not required or bool(value), f"{context}: evidence sources are required")
    require(
        len(value) <= HARD_MAXIMA["sources_per_record"],
        f"{context}: exceeds hard source-per-record maximum",
    )
    for index, source in enumerate(value):
        validate_source(source, sources, f"{context}[{index}]")
    require(value == canonical_sources(value), f"{context}: sources must be sorted and canonical")
    require(
        len({(item["path"], item["blob"], item["sha256"], item["span"]) for item in value})
        == len(value),
        f"{context}: duplicate source",
    )
    return value


def validate_repo_source(source: object, context: str) -> tuple[str, dict]:
    require(isinstance(source, dict), f"{context}: inventory source must be an object")
    require(
        set(source) == {"path", "blob", "sha256", "bytes", "class", "decision"},
        f"{context}: inventory source fields must be exact",
    )
    path = safe_relative_path(source.get("path"), context)
    require(
        isinstance(source.get("blob"), str)
        and HEX40_OR_64.fullmatch(source["blob"]) is not None,
        f"{context}: invalid blob",
    )
    require(
        isinstance(source.get("sha256"), str) and HEX64.fullmatch(source["sha256"]) is not None,
        f"{context}: invalid sha256",
    )
    require(
        isinstance(source.get("bytes"), int)
        and not isinstance(source["bytes"], bool)
        and source["bytes"] >= 0,
        f"{context}: invalid bytes",
    )
    require(isinstance(source.get("class"), str) and bool(source["class"]), f"{context}: class is required")
    require(source.get("decision") in {"inspect", "evidence", "deferred"}, f"{context}: invalid decision")
    return path, source


def validate_manifest_base(
    root: Path,
    base: dict,
    *,
    allow_zero_generation_postcondition: bool = False,
) -> dict:
    generation = base.get("manifest_generation")
    require(
        isinstance(generation, int) and not isinstance(generation, bool) and generation >= 0,
        "invalid base.manifest_generation",
    )
    manifest_hash = base.get("manifest_hash")
    require(
        manifest_hash is None
        or (isinstance(manifest_hash, str) and HEX64.fullmatch(manifest_hash) is not None),
        "invalid base.manifest_hash",
    )
    relative = safe_relative_path(base.get("manifest_path"), "base.manifest_path")
    try:
        manifest_path, canonical_relative = resolve_sidecar_path(
            root, relative, "base.manifest_path"
        )
    except InventoryError as error:
        raise PlanError(str(error)) from error
    require(
        canonical_relative == relative,
        "base.manifest_path must be canonical and repository-relative",
    )
    try:
        manifest_path = resolve_manifest_authority(
            root,
            manifest_path,
            canonical_relative,
            allow_unactivated_catalog=allow_zero_generation_postcondition,
        )
    except ManifestContractError as error:
        raise PlanError(str(error)) from error
    if not manifest_path.exists():
        require(generation == 0 and manifest_hash is None, "stale plan: manifest is absent but base is nonzero")
        return {}
    if allow_zero_generation_postcondition:
        require(
            generation == 0 and manifest_hash is None,
            "only an absent greenfield base may be checked as a published postcondition",
        )
        # The journal validator separately parses this file and proves it is the
        # exact deterministic postcondition. It is not treated as the plan's base.
        return {}
    try:
        status, manifest = parse_manifest(manifest_path, root)
    except InventoryError as error:
        raise PlanError(f"cannot read base manifest {manifest_path}: {error}") from error
    require(status == "valid", "base manifest is absent")
    require(manifest["generation"] == generation, "stale plan: manifest generation changed")
    actual_hash = canonical_sha256(manifest)
    require(manifest_hash == actual_hash, f"stale plan: manifest hash changed; expected {actual_hash}")
    return manifest


def build_unique_rename_map(
    root: Path,
    manifest: dict,
    records_by_path: dict[str, dict],
    ignored: set[str],
    blob_cache: dict[str, bytes],
) -> tuple[dict[str, str], set[str]]:
    """Prove exact same-blob renames and fail closed on every ambiguity."""

    old_files = manifest.get("files", {})
    missing = {path for path in old_files if path not in records_by_path}
    paths_by_blob: dict[str, list[str]] = {}
    for path, record in records_by_path.items():
        if path in old_files or exclusion_reason(record, DEFAULT_MAX_BYTES, ignored) is not None:
            continue
        if record["oid"] not in {old_files[old]["blob"] for old in missing}:
            continue
        content = blob_cache.get(record["oid"])
        if content is None:
            content = read_blob(root, record["oid"])
            blob_cache[record["oid"]] = content
        if content_exclusion_reason(record, content) is None:
            paths_by_blob.setdefault(record["oid"], []).append(path)
    rename_map: dict[str, str] = {}
    deleted: set[str] = set()
    old_by_blob: dict[str, list[str]] = {}
    for old in missing:
        old_by_blob.setdefault(old_files[old]["blob"], []).append(old)
    for blob, old_paths in sorted(old_by_blob.items()):
        new_paths = sorted(paths_by_blob.get(blob, []))
        old_paths = sorted(old_paths)
        if len(old_paths) == 1 and len(new_paths) == 1:
            rename_map[old_paths[0]] = new_paths[0]
        elif new_paths:
            raise PlanError(
                f"ambiguous managed-source rename for blob {blob}: {old_paths!r} -> {new_paths!r}"
            )
        else:
            deleted.update(old_paths)
    return rename_map, deleted


def verify_rename_only(old_sources: list[dict], new_sources: list[dict], rename_map: dict[str, str], context: str) -> None:
    expected = sorted(
        (
            rename_map.get(source["path"], source["path"]),
            source["blob"],
            source["sha256"],
            source["span"],
        )
        for source in old_sources
    )
    actual = sorted(
        (source["path"], source["blob"], source["sha256"], source["span"])
        for source in new_sources
    )
    require(actual == expected, f"{context}: noop evidence is not an exact proven path-only rename")


def validate_deleted_sources(
    value: object,
    owned_sources: list[dict],
    tree_blobs: dict[str, str],
    rename_map: dict[str, str],
    context: str,
) -> None:
    owned_paths = sorted({source["path"] for source in owned_sources})
    require(
        isinstance(value, list) and bool(value) and all(isinstance(path, str) for path in value),
        f"{context}: deleted_source_paths must be a nonempty array",
    )
    require(value == sorted(set(value)), f"{context}: deleted_source_paths must be sorted and unique")
    require(value == owned_paths, f"{context}: deleted_source_paths must match owned evidence")
    require(all(path not in tree_blobs for path in value), f"{context}: a purportedly deleted source still exists")
    require(all(path not in rename_map for path in value), f"{context}: a purported deletion is a proven rename")


def current_node(manifest_nodes: dict, key: str) -> dict | None:
    record = manifest_nodes.get(key)
    return record.get("current") if isinstance(record, dict) else None


def current_edge(manifest_edges: dict, key: str) -> dict | None:
    record = manifest_edges.get(key)
    return record.get("current") if isinstance(record, dict) else None


def validate(
    plan: object,
    root: Path,
    *,
    raw_size: int | None = None,
    allow_greenfield_postcondition: bool = False,
) -> dict:
    require(isinstance(plan, dict), "plan root must be an object")
    detected_secret = secret_summary(canonical_bytes(plan))
    require(
        detected_secret is None,
        f"plan contains high-confidence secret material ({detected_secret})",
    )
    if raw_size is not None:
        require(raw_size <= HARD_MAXIMA["plan_bytes"], "plan exceeds hard 2 MiB maximum")
    required_plan_fields = {
        "schema_version",
        "namespace",
        "policy_version",
        "mode",
        "target",
        "repo",
        "base",
        "inventory",
        "limits",
        "sources",
        "nodes",
        "edges",
        "brownfield_dispositions",
        "adversarial_findings",
        "exclusions",
        "post_manifest_delta",
        "plan_hash",
    }
    require(set(plan) == required_plan_fields, "plan fields must match the v1 contract exactly")
    require(plan.get("schema_version") == SCHEMA_VERSION, f"schema_version must be {SCHEMA_VERSION}")
    require(plan.get("namespace") == NAMESPACE, f"namespace must be {NAMESPACE}")
    require(plan.get("policy_version") == POLICY_VERSION, "unsupported hard-policy version")
    mode = plan.get("mode")
    require(mode in MODES, "invalid plan mode")

    target = plan.get("target")
    require(isinstance(target, dict) and set(target) == {"db_id", "expected_empty"}, "target fields must be exact")
    require(isinstance(target.get("db_id"), str) and ULID.fullmatch(target["db_id"]) is not None, "invalid target.db_id")
    require(isinstance(target.get("expected_empty"), bool), "target.expected_empty must be boolean")
    require(target["expected_empty"] == (mode == "greenfield"), "target empty precondition disagrees with mode")

    repo = plan.get("repo")
    require(isinstance(repo, dict), "repo must be an object")
    require(set(repo) == {"head", "tree", "object_format", "dirty_digest"}, "repo fields must be exact")
    head, tree = repo.get("head"), repo.get("tree")
    require(isinstance(head, str) and HEX40_OR_64.fullmatch(head) is not None, "invalid repo.head")
    require(isinstance(tree, str) and HEX40_OR_64.fullmatch(tree) is not None, "invalid repo.tree")
    require(repo.get("object_format") in {"sha1", "sha256"}, "invalid repo.object_format")
    require(repo.get("dirty_digest") is None, "v1 accepts committed-tree plans only")
    current_head = git(root, "rev-parse", "--verify", "HEAD^{commit}")
    require(current_head == head, "stale plan: repository HEAD changed")
    require(git(root, "rev-parse", f"{head}^{{tree}}") == tree, "stale plan: repository tree changed")
    require(git(root, "rev-parse", "--show-object-format") == repo["object_format"], "Git object format changed")

    base = plan.get("base")
    require(isinstance(base, dict), "base must be an object")
    require(set(base) == {"manifest_path", "manifest_generation", "manifest_hash"}, "base fields must be exact")
    manifest = validate_manifest_base(
        root,
        base,
        allow_zero_generation_postcondition=(
            allow_greenfield_postcondition and mode == "greenfield"
        ),
    )
    if mode == "managed_refresh_proposal":
        require(bool(manifest), "managed_refresh_proposal requires a valid manifest")
        require(manifest["db_id"] == target["db_id"], "target db_id disagrees with manifest")
        ancestor = subprocess.run(
            ["git", "--no-replace-objects", "-C", str(root), "merge-base", "--is-ancestor", manifest["repo"]["head"], head],
            stdout=subprocess.PIPE,
            stderr=subprocess.PIPE,
            check=False,
            env={
                **os.environ,
                "GIT_NO_REPLACE_OBJECTS": "1",
                "GIT_GRAFT_FILE": os.devnull,
            },
        )
        require(ancestor.returncode == 0, "manifest source cut is not an ancestor of plan HEAD")
    else:
        require(not manifest, f"{mode} requires an absent manifest")

    inventory_binding = plan.get("inventory")
    require(
        isinstance(inventory_binding, dict)
        and set(inventory_binding) == {"path", "sha256"},
        "inventory binding fields must be exact",
    )
    try:
        inventory_path, inventory_relative = resolve_sidecar_path(
            root, inventory_binding.get("path"), "inventory.path"
        )
        inventory = load_inventory_artifact(inventory_path)
    except (InventoryError, TypeError) as error:
        raise PlanError(f"invalid bound inventory: {error}") from error
    require(
        inventory_binding["path"] == inventory_relative,
        "inventory.path must be canonical and repository-relative",
    )
    require(
        inventory_binding.get("sha256") == inventory["inventory_hash"],
        "inventory binding sha256 disagrees with artifact",
    )
    require(
        inventory["repo"]
        == {"head": head, "tree": tree, "object_format": repo["object_format"]},
        "bound inventory source cut disagrees with plan.repo",
    )
    require(
        inventory["manifest"]
        == {
            "path": base["manifest_path"],
            "status": "valid" if manifest else "absent",
            "generation": base["manifest_generation"] if manifest else None,
            "db_id": manifest.get("db_id") if manifest else None,
            "sha256": base["manifest_hash"],
        },
        "bound inventory manifest cut disagrees with plan.base",
    )
    inventory_limits = inventory.get("limits")
    require(
        isinstance(inventory_limits, dict)
        and set(inventory_limits) == {"max_files", "max_bytes"}
        and all(
            isinstance(inventory_limits[field], int)
            and not isinstance(inventory_limits[field], bool)
            for field in ("max_files", "max_bytes")
        ),
        "bound inventory limits are invalid",
    )
    if not (allow_greenfield_postcondition and mode == "greenfield"):
        try:
            reconstructed_inventory = construct_inventory(
                root,
                (root / base["manifest_path"]).resolve(strict=False),
                base["manifest_path"],
                max_files=inventory_limits["max_files"],
                max_bytes=inventory_limits["max_bytes"],
            )
        except InventoryError as error:
            raise PlanError(f"cannot reconstruct bound inventory: {error}") from error
        require(
            reconstructed_inventory == inventory,
            "bound inventory is not the complete canonical inventory for this source cut",
        )

    limits = plan.get("limits")
    required_limits = {"nodes", "edges", "bytes_per_body", "max_out_degree", "max_in_degree"}
    require(isinstance(limits, dict) and set(limits) == required_limits, "limits fields must be exact")
    for field in sorted(required_limits):
        value = limits.get(field)
        require(isinstance(value, int) and not isinstance(value, bool) and value > 0, f"limits.{field} must be positive")
        require(value <= HARD_MAXIMA[field], f"limits.{field} exceeds hard policy maximum {HARD_MAXIMA[field]}")

    raw_sources = plan.get("sources")
    require(isinstance(raw_sources, list), "sources must be an array")
    require(len(raw_sources) <= HARD_MAXIMA["sources"], "source inventory exceeds hard maximum")
    records = tree_records(root, head)
    records_by_path = {record["path"]: record for record in records}
    ignored = ignored_paths(root, records_by_path)
    blob_cache: dict[str, bytes] = {}
    sources: dict[str, dict] = {}
    for index, source in enumerate(raw_sources):
        path, value = validate_repo_source(source, f"sources[{index}]")
        require(path not in sources, f"duplicate source path {path!r}")
        record = records_by_path.get(path)
        require(record is not None and record.get("oid") == value["blob"], f"sources[{index}]: blob is not HEAD:{path}")
        reason = exclusion_reason(record, DEFAULT_MAX_BYTES, ignored)
        require(reason is None, f"sources[{index}]: ineligible source ({reason})")
        content = blob_cache.get(value["blob"])
        if content is None:
            content = read_blob(root, value["blob"])
            blob_cache[value["blob"]] = content
        reason = content_exclusion_reason(record, content)
        require(reason is None, f"sources[{index}]: ineligible source ({reason})")
        require(value["bytes"] == len(content), f"sources[{index}]: byte count disagrees with blob")
        require(value["sha256"] == hashlib.sha256(content).hexdigest(), f"sources[{index}]: sha256 disagrees with blob")
        expected_class, _priority = classify(path)
        require(value["class"] == expected_class, f"sources[{index}]: source class disagrees with inventory")
        line_count = content.count(b"\n") + int(bool(content) and not content.endswith(b"\n"))
        sources[path] = {**value, "line_count": line_count}
    require(list(sources) == sorted(sources), "sources must be sorted by path")
    expected_inventory_sources = {
        item["path"]: {
            "path": item["path"],
            "blob": item["blob"],
            "sha256": item["sha256"],
            "bytes": item["bytes"],
            "class": item["class"],
        }
        for item in inventory["files"]
    }
    require(
        set(sources) == set(expected_inventory_sources),
        "plan sources must exactly cover every selected inventory file",
    )
    for path, expected in expected_inventory_sources.items():
        require(
            {field: sources[path][field] for field in expected} == expected,
            f"plan source {path!r} disagrees with bound inventory",
        )
    tree_blobs = {path: record["oid"] for path, record in records_by_path.items()}

    rename_map: dict[str, str] = {}
    deleted_manifest_paths: set[str] = set()
    if manifest:
        rename_map, deleted_manifest_paths = build_unique_rename_map(
            root, manifest, records_by_path, ignored, blob_cache
        )
        for path, old in manifest["files"].items():
            if path not in records_by_path:
                continue
            record = records_by_path[path]
            reason = exclusion_reason(record, DEFAULT_MAX_BYTES, ignored)
            require(reason is None, f"managed source {path!r} became ineligible ({reason})")
            content = blob_cache.get(record["oid"])
            if content is None:
                content = read_blob(root, record["oid"])
                blob_cache[record["oid"]] = content
            reason = content_exclusion_reason(record, content)
            require(reason is None, f"managed source {path!r} became ineligible ({reason})")

    manifest_nodes = manifest.get("nodes", {})
    manifest_edges = manifest.get("edges", {})
    raw_nodes = plan.get("nodes")
    require(isinstance(raw_nodes, list), "nodes must be an array")
    require(len(raw_nodes) <= limits["nodes"], "node plan exceeds declared limit")
    require(len(raw_nodes) <= HARD_MAXIMA["nodes"], "node plan exceeds hard maximum")
    nodes: dict[str, dict] = {}
    core_count = 0
    mode_node_actions = {
        "greenfield": {"ingest"},
        "managed_refresh_proposal": {"noop", "ingest_proposal", "replace_proposal", "retirement_proposal"},
        "brownfield_proposal": {"ingest_proposal", "adoption_proposal"},
    }[mode]
    for index, node in enumerate(raw_nodes):
        context = f"nodes[{index}]"
        require(isinstance(node, dict), f"{context}: node must be an object")
        key, action = node.get("key"), node.get("action")
        require(isinstance(key, str) and KEY.fullmatch(key) is not None, f"{context}: invalid key")
        require(key not in nodes, f"duplicate node key {key!r}")
        require(action in mode_node_actions, f"{context}: action {action!r} is forbidden in mode {mode}")
        managed = current_node(manifest_nodes, key)
        if action == "retirement_proposal":
            expected_fields = {
                "key", "previous_node_id", "previous_content_hash", "previous_materialization_hash",
                "deleted_source_paths", "reason", "action",
            }
            require(set(node) == expected_fields, f"{context}: retirement fields must be exact")
            require(managed is not None, f"{context}: retirement target is not current and manifest-owned")
            require(node["previous_node_id"] == managed["node_id"], f"{context}: previous_node_id mismatch")
            require(node["previous_content_hash"] == managed["content_hash"], f"{context}: previous_content_hash mismatch")
            require(node["previous_materialization_hash"] == managed["materialization_hash"], f"{context}: previous_materialization_hash mismatch")
            reason = node.get("reason")
            require(isinstance(reason, str) and bool(reason) and reason == normalize_text(reason), f"{context}: reason is blank or noncanonical")
            utf8_bytes(reason, f"{context}.reason")
            validate_deleted_sources(node.get("deleted_source_paths"), managed["evidence"], tree_blobs, rename_map, context)
            nodes[key] = node
            continue
        if action == "noop":
            expected_fields = {
                "key", "expected_node_id", "expected_content_hash", "expected_materialization_hash",
                "sources", "action",
            }
            require(set(node) == expected_fields, f"{context}: noop fields must be exact")
            require(managed is not None, f"{context}: noop target is not current and manifest-owned")
            require(node["expected_node_id"] == managed["node_id"], f"{context}: expected_node_id mismatch")
            require(node["expected_content_hash"] == managed["content_hash"], f"{context}: expected_content_hash mismatch")
            require(node["expected_materialization_hash"] == managed["materialization_hash"], f"{context}: expected_materialization_hash mismatch")
            node_sources = validate_source_list(node.get("sources"), sources, f"{context}.sources", required=True)
            verify_rename_only(managed["evidence"], node_sources, rename_map, context)
            expected_hash = node_content_hash(key=key, summary=managed["summary"], claim=managed["claim"], sources=node_sources)
            require(expected_hash == managed["content_hash"], f"{context}: noop semantic content changed")
            core_count += int("core" in managed["tags"])
            nodes[key] = node
            continue

        expected_fields = {
            "key", "content_hash", "materialization_hash", "summary", "body", "tags", "status",
            "stability", "confidence", "sources", "action",
        }
        if action == "replace_proposal":
            expected_fields |= {"previous_node_id", "previous_content_hash"}
        require(set(node) == expected_fields, f"{context}: fields do not match action {action}")
        if action == "replace_proposal":
            require(managed is not None, f"{context}: replacement target is not current and manifest-owned")
            require(node["previous_node_id"] == managed["node_id"], f"{context}: previous_node_id mismatch")
            require(node["previous_content_hash"] == managed["content_hash"], f"{context}: previous_content_hash mismatch")
        else:
            require(key not in manifest_nodes, f"{context}: proposed new key collides with manifest ownership")
        require(node.get("status") in STATUSES, f"{context}: invalid status")
        summary = node.get("summary")
        require(isinstance(summary, str) and bool(summary) and summary == normalize_text(summary), f"{context}: summary is blank or noncanonical")
        require(len(utf8_bytes(summary, f"{context}.summary")) <= HARD_MAXIMA["summary_bytes"], f"{context}: summary exceeds hard maximum")
        body = node.get("body")
        require(isinstance(body, str) and bool(body), f"{context}: blank body")
        require(len(utf8_bytes(body, f"{context}.body")) <= limits["bytes_per_body"], f"{context}: body exceeds limit")
        tags = node.get("tags")
        require(isinstance(tags, list) and all(isinstance(tag, str) and bool(tag) for tag in tags), f"{context}: invalid tags")
        require(tags == sorted(set(tags)), f"{context}: tags must be sorted and unique")
        require(len(tags) <= HARD_MAXIMA["tags_per_node"], f"{context}: too many tags")
        require(
            all(
                not has_forbidden_controls(tag)
                and len(utf8_bytes(tag, f"{context}.tags")) <= HARD_MAXIMA["tag_bytes"]
                for tag in tags
            ),
            f"{context}: tag has a forbidden control character or exceeds hard maximum",
        )
        require(NAMESPACE in tags, f"{context}: missing namespace tag")
        if "core" in tags:
            require(node["status"] == "active", f"{context}: core must be active")
            core_count += 1
        stability = unit_number(node.get("stability"), f"{context}.stability")
        confidence = unit_number(node.get("confidence"), f"{context}.confidence")
        node_sources = validate_source_list(node.get("sources"), sources, f"{context}.sources", required=True)
        if "core" in tags:
            require(
                not any(is_agent_instruction_path(source["path"]) for source in node_sources),
                f"{context}: agent instruction evidence can never auto-core a node",
            )
        try:
            header, claim = parse_body(body)
        except ValueError as error:
            raise PlanError(f"{context}: {error}") from error
        require(header["namespace"] == NAMESPACE and header["key"] == key, f"{context}: recovery identity mismatch")
        require(header["source-commit"] == head, f"{context}: recovery source commit mismatch")
        require(
            header["extractor-version"] == str(EXTRACTOR_VERSION)
            and header["source-state"] == "git-committed"
            and header["content-trust"] == "untrusted-evidence",
            f"{context}: recovery metadata mismatch",
        )
        require(bool(claim), f"{context}: claim must not be blank")
        if "core" in tags:
            require(
                len(utf8_bytes(body, f"{context}.body"))
                <= HARD_MAXIMA["core_body_bytes"],
                f"{context}: core body exceeds hard {HARD_MAXIMA['core_body_bytes']} byte maximum",
            )
        expected_content = node_content_hash(key=key, summary=summary, claim=claim, sources=node_sources)
        require(node.get("content_hash") == expected_content, f"{context}: content_hash mismatch; expected {expected_content}")
        require(header["content-sha256"] == expected_content, f"{context}: recovery content hash mismatch")
        expected_materialization = materialization_hash(
            content_hash=expected_content,
            body=body,
            tags=tags,
            status=node["status"],
            stability=stability,
            confidence=confidence,
        )
        require(node.get("materialization_hash") == expected_materialization, f"{context}: materialization_hash mismatch")
        if action == "replace_proposal":
            require(
                expected_content != managed["content_hash"]
                or expected_materialization != managed["materialization_hash"],
                f"{context}: unchanged replacement must be a noop",
            )
        nodes[key] = node
    require(list(nodes) == sorted(nodes), "nodes must be sorted by key")
    changed_count = sum(node["action"] != "noop" for node in nodes.values())
    if mode == "managed_refresh_proposal":
        require(changed_count <= HARD_MAXIMA["changed_nodes"], "managed refresh exceeds hard changed-node maximum")
    if mode == "greenfield":
        require(core_count == 1, "greenfield plan must contain exactly one active core node")
    else:
        resulting_core_keys = {
            key
            for key, record in manifest_nodes.items()
            if record.get("current") is not None and "core" in record["current"]["tags"]
        }
        for key, node in nodes.items():
            action = node["action"]
            if action in {"retirement_proposal", "replace_proposal"}:
                resulting_core_keys.discard(key)
            if action in {"ingest_proposal", "adoption_proposal", "replace_proposal"}:
                if "core" in node["tags"]:
                    resulting_core_keys.add(key)
        require(
            len(resulting_core_keys) <= 1,
            "resulting proposal contains more than one core node",
        )

    # Every changed, deleted, or renamed owned record must be accounted for.
    if manifest:
        affected_nodes = set()
        affected_edges = set()
        for key, record in manifest_nodes.items():
            managed = record["current"]
            if managed is None:
                continue
            for source in managed["evidence"]:
                path = source["path"]
                current = records_by_path.get(path)
                if path in rename_map or path in deleted_manifest_paths or current is None or current["oid"] != source["blob"]:
                    affected_nodes.add(key)
                    break
        for key, record in manifest_edges.items():
            managed = record["current"]
            if managed is None:
                continue
            for source in managed["evidence"]:
                path = source["path"]
                current = records_by_path.get(path)
                if path in rename_map or path in deleted_manifest_paths or current is None or current["oid"] != source["blob"]:
                    affected_edges.add(key)
                    break
        require(affected_nodes <= set(nodes), f"plan omits affected managed nodes: {sorted(affected_nodes - set(nodes))!r}")

    raw_edges = plan.get("edges")
    require(isinstance(raw_edges, list), "edges must be an array")
    require(len(raw_edges) <= limits["edges"], "edge plan exceeds declared limit")
    require(len(raw_edges) <= HARD_MAXIMA["edges"], "edge plan exceeds hard maximum")
    edges: dict[str, dict] = {}
    directed_pairs: dict[tuple[str, str], str] = {}
    undirected_pairs: dict[tuple[str, str], str] = {}
    mode_edge_actions = {
        "greenfield": {"link"},
        "managed_refresh_proposal": {"noop", "link_proposal", "replace_proposal", "retirement_proposal"},
        "brownfield_proposal": {"link_proposal"},
    }[mode]
    for index, edge in enumerate(raw_edges):
        context = f"edges[{index}]"
        require(isinstance(edge, dict), f"{context}: edge must be an object")
        key, action = edge.get("key"), edge.get("action")
        require(isinstance(key, str) and EDGE_KEY.fullmatch(key) is not None, f"{context}: invalid key")
        require(key not in edges, f"duplicate edge key {key!r}")
        require(action in mode_edge_actions, f"{context}: action {action!r} is forbidden in mode {mode}")
        require(edge.get("kind") in BOOTSTRAP_EDGE_KINDS, f"{context}: invalid kind")
        from_key, to_key = edge.get("from_key"), edge.get("to_key")
        require(isinstance(from_key, str) and isinstance(to_key, str), f"{context}: endpoint keys are required")
        require(from_key != to_key, f"{context}: self-loop assertions are forbidden")
        endpoint_keys = set(nodes) | {k for k, record in manifest_nodes.items() if record["current"] is not None}
        require(from_key in endpoint_keys and to_key in endpoint_keys, f"{context}: unknown endpoint key")
        require(nodes.get(from_key, {}).get("action") != "retirement_proposal" and nodes.get(to_key, {}).get("action") != "retirement_proposal", f"{context}: retiring node cannot be an edge endpoint")
        if action == "link":
            require(nodes.get(from_key, {}).get("action") == "ingest" and nodes.get(to_key, {}).get("action") == "ingest", f"{context}: executable edge endpoints must be executable ingests")
        managed = current_edge(manifest_edges, key)
        if action == "retirement_proposal":
            expected_fields = {
                "key", "from_key", "to_key", "kind", "previous_assertion_hash",
                "deleted_source_paths", "reason", "action",
            }
            require(set(edge) == expected_fields, f"{context}: retirement fields must be exact")
            require(managed is not None, f"{context}: retirement target is not current and manifest-owned")
            for field in ("from_key", "to_key", "kind"):
                require(edge[field] == managed[field], f"{context}: {field} mismatch")
            require(edge["previous_assertion_hash"] == managed["assertion_hash"], f"{context}: previous_assertion_hash mismatch")
            reason = edge.get("reason")
            require(isinstance(reason, str) and bool(reason) and reason == normalize_text(reason), f"{context}: reason is blank or noncanonical")
            utf8_bytes(reason, f"{context}.reason")
            validate_deleted_sources(edge.get("deleted_source_paths"), managed["evidence"], tree_blobs, rename_map, context)
            edges[key] = edge
            continue

        expected_fields = {"key", "from_key", "to_key", "kind", "weight", "assertion_hash", "sources", "action"}
        if action == "replace_proposal":
            expected_fields.add("previous_assertion_hash")
        require(set(edge) == expected_fields, f"{context}: fields do not match action {action}")
        weight = unit_number(edge.get("weight"), f"{context}.weight")
        edge_sources = validate_source_list(edge.get("sources"), sources, f"{context}.sources", required=True)
        expected_assertion = edge_assertion_hash(
            key=key,
            from_key=from_key,
            to_key=to_key,
            kind=edge["kind"],
            weight=weight,
            sources=edge_sources,
        )
        require(edge.get("assertion_hash") == expected_assertion, f"{context}: assertion_hash mismatch")
        if action == "noop":
            require(managed is not None, f"{context}: noop target is not current and manifest-owned")
            for field in ("from_key", "to_key", "kind", "weight", "assertion_hash"):
                require(edge[field] == managed[field], f"{context}: noop {field} mismatch")
            verify_rename_only(managed["evidence"], edge_sources, rename_map, context)
        elif action == "replace_proposal":
            require(managed is not None, f"{context}: replacement target is not current and manifest-owned")
            require(edge["previous_assertion_hash"] == managed["assertion_hash"], f"{context}: previous_assertion_hash mismatch")
            require(expected_assertion != managed["assertion_hash"], f"{context}: unchanged replacement must be a noop")
        else:
            require(key not in manifest_edges, f"{context}: proposed new key collides with manifest ownership")

        pair = (from_key, to_key)
        require(pair not in directed_pairs, f"{context}: endpoint pair already asserted by {directed_pairs.get(pair)!r}")
        directed_pairs[pair] = key
        unordered = tuple(sorted(pair))
        if edge["kind"] == "associative":
            require(unordered not in undirected_pairs, f"{context}: associative pair is duplicated in reverse")
            undirected_pairs[unordered] = key
        edges[key] = edge
    require(list(edges) == sorted(edges), "edges must be sorted by key")
    if manifest:
        require(affected_edges <= set(edges), f"plan omits affected managed edges: {sorted(affected_edges - set(edges))!r}")

    # Pair uniqueness and degree apply to the retained-plus-proposed topology.
    resulting: dict[str, tuple[str, str, str]] = {}
    for key, record in manifest_edges.items():
        if record["current"] is not None:
            current = record["current"]
            resulting[key] = (current["from_key"], current["to_key"], current["kind"])
    for key, edge in edges.items():
        if edge["action"] == "retirement_proposal":
            resulting.pop(key, None)
        elif edge["action"] in {"link", "link_proposal", "replace_proposal", "noop"}:
            resulting[key] = (edge["from_key"], edge["to_key"], edge["kind"])
    resulting_node_keys = {
        key for key, record in manifest_nodes.items() if record["current"] is not None
    }
    for key, node in nodes.items():
        if node["action"] == "retirement_proposal":
            resulting_node_keys.discard(key)
        else:
            resulting_node_keys.add(key)
    pair_owner: dict[tuple[str, str], str] = {}
    associative_owner: dict[tuple[str, str], str] = {}
    out_degree: Counter[str] = Counter()
    in_degree: Counter[str] = Counter()
    for key, (from_key, to_key, kind) in sorted(resulting.items()):
        require(
            from_key in resulting_node_keys and to_key in resulting_node_keys,
            f"resulting edge {key!r} targets a retired or absent node",
        )
        pair = (from_key, to_key)
        require(pair not in pair_owner, f"resulting topology duplicates endpoint pair {pair!r}")
        reverse = (to_key, from_key)
        unordered = tuple(sorted(pair))
        if unordered in associative_owner:
            raise PlanError(f"resulting topology conflicts with associative pair {unordered!r}")
        if kind == "associative":
            require(reverse not in pair_owner, f"resulting topology duplicates associative pair {unordered!r}")
            associative_owner[unordered] = key
        pair_owner[pair] = key
        out_degree[from_key] += 1
        in_degree[to_key] += 1
    require(max(out_degree.values(), default=0) <= limits["max_out_degree"], "resulting topology exceeds max_out_degree")
    require(max(in_degree.values(), default=0) <= limits["max_in_degree"], "resulting topology exceeds max_in_degree")

    dispositions = plan.get("brownfield_dispositions")
    require(isinstance(dispositions, list), "brownfield_dispositions must be an array")
    require(len(dispositions) <= HARD_MAXIMA["brownfield_dispositions"], "too many brownfield dispositions")
    require(mode == "brownfield_proposal" or not dispositions, "brownfield dispositions are only valid in brownfield mode")
    disposition_ids: list[str] = []
    adoption_counts: Counter[str] = Counter()
    for index, item in enumerate(dispositions):
        context = f"brownfield_dispositions[{index}]"
        require(isinstance(item, dict), f"{context}: must be an object")
        require(
            set(item)
            == {
                "legacy_node_id", "legacy_status", "legacy_summary_sha256", "legacy_record_sha256",
                "disposition", "managed_key", "reason",
            },
            f"{context}: fields must be exact",
        )
        node_id = item.get("legacy_node_id")
        require(isinstance(node_id, str) and ULID.fullmatch(node_id) is not None, f"{context}: invalid legacy_node_id")
        disposition_ids.append(node_id)
        require(item.get("legacy_status") in {"active", "candidate", "archived"}, f"{context}: invalid legacy_status")
        for field in ("legacy_summary_sha256", "legacy_record_sha256"):
            require(isinstance(item.get(field), str) and HEX64.fullmatch(item[field]) is not None, f"{context}: invalid {field}")
        require(item.get("disposition") in DISPOSITIONS, f"{context}: invalid disposition")
        reason = item.get("reason")
        require(isinstance(reason, str) and bool(reason) and reason == normalize_text(reason), f"{context}: reason is blank or noncanonical")
        utf8_bytes(reason, f"{context}.reason")
        managed_key = item.get("managed_key")
        if item["disposition"] == "leave":
            require(managed_key is None, f"{context}: leave cannot name managed_key")
        else:
            require(isinstance(managed_key, str) and KEY.fullmatch(managed_key) is not None, f"{context}: proposal requires managed_key")
            require(managed_key in nodes, f"{context}: managed_key is absent from brownfield proposal")
            if item["disposition"] == "adoption_proposal":
                require(nodes[managed_key]["action"] == "adoption_proposal", f"{context}: adoption requires matching node proposal")
                adoption_counts[managed_key] += 1
    require(disposition_ids == sorted(disposition_ids) and len(set(disposition_ids)) == len(disposition_ids), "brownfield dispositions must be unique and sorted")
    proposed_adoptions = {key for key, node in nodes.items() if node["action"] == "adoption_proposal"}
    require(proposed_adoptions == set(adoption_counts), "every adoption node must match one reviewed legacy record")
    require(all(count == 1 for count in adoption_counts.values()), "an adoption proposal must map exactly one legacy record")

    findings = plan.get("adversarial_findings")
    require(isinstance(findings, list), "adversarial_findings must be an array")
    require(len(findings) <= HARD_MAXIMA["findings"], "too many adversarial findings")
    finding_order = []
    for index, item in enumerate(findings):
        context = f"adversarial_findings[{index}]"
        require(
            isinstance(item, dict)
            and set(item) == {"kind", "status", "detail", "disposition", "sources"},
            f"{context}: fields must be exact",
        )
        require(item.get("kind") in FINDING_KINDS, f"{context}: invalid kind")
        require(item.get("status") in FINDING_STATUSES, f"{context}: invalid status")
        for field in ("detail", "disposition"):
            require(isinstance(item.get(field), str) and bool(item[field]) and item[field] == normalize_text(item[field]), f"{context}: {field} is blank or noncanonical")
            utf8_bytes(item[field], f"{context}.{field}")
        validate_source_list(item.get("sources"), sources, f"{context}.sources", required=False)
        finding_order.append((item["kind"], item["detail"]))
    require(finding_order == sorted(set(finding_order)), "adversarial findings must be unique and sorted")
    blocking_findings = [
        item
        for item in findings
        if item["kind"] in APPLY_BLOCKING_FINDINGS and item["status"] == "unresolved"
    ]
    adversarial_review_complete = bool(findings)
    blocking_reasons = [
        f"unresolved_{item['kind']}" for item in blocking_findings
    ]
    if not adversarial_review_complete:
        blocking_reasons.append("adversarial_review_missing")

    exclusions = plan.get("exclusions")
    require(
        isinstance(exclusions, dict) and set(exclusions) == {"count", "sha256"},
        "exclusions must be an exact inventory digest binding",
    )
    require(
        exclusions
        == {
            "count": len(inventory["exclusions"]),
            "sha256": inventory["exclusions_sha256"],
        },
        "exclusions digest disagrees with bound complete inventory",
    )

    expected_delta = expected_post_manifest_delta(plan)
    require(plan.get("post_manifest_delta") == expected_delta, "post_manifest_delta disagrees with canonical actions")
    require(expected_delta["publish"] == (mode == "greenfield"), "only greenfield may publish a manifest")
    expected_hash = plan_hash(plan)
    require(plan.get("plan_hash") == expected_hash, f"plan_hash mismatch; expected {expected_hash}")
    require(
        git(root, "rev-parse", "--verify", "HEAD^{commit}") == head
        and git(root, "rev-parse", f"{head}^{{tree}}") == tree,
        "repository source cut changed while plan was being validated",
    )
    if not (allow_greenfield_postcondition and mode == "greenfield"):
        final_manifest = validate_manifest_base(root, base)
        require(final_manifest == manifest, "manifest changed while plan was being validated")
    return {
        "valid": True,
        "structurally_valid": True,
        "plan_hash": expected_hash,
        "mode": mode,
        "apply_allowed": False,
        "native_bootstrap_create_required": mode == "greenfield",
        "apply_blocked_reason": (
            "current public APIs cannot produce an authenticated native bootstrap receipt"
            if mode == "greenfield"
            else "proposal modes are never executable"
        ),
        "adversarial_review_complete": adversarial_review_complete,
        "blocking_findings": len(blocking_reasons),
        "blocking_reasons": blocking_reasons,
        "live_precondition_required": (
            "future native bootstrap-create must prove an absent target, fixed topology-off "
            "policy, and exact final projection"
        ),
        "sources": len(sources),
        "nodes": len(nodes),
        "edges": len(edges),
        "head": head,
        "tree": tree,
    }


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("plan", type=Path)
    parser.add_argument("--root", default=".")
    parser.add_argument("--expected-hash", action="store_true")
    parser.add_argument(
        "--native-greenfield-postcondition",
        action="store_true",
        help=argparse.SUPPRESS,
    )
    args = parser.parse_args()
    root = Path(args.root).resolve()
    try:
        raw = sys.stdin.buffer.read() if str(args.plan) == "-" else args.plan.read_bytes()
        if len(raw) > HARD_MAXIMA["plan_bytes"]:
            raise PlanError("plan exceeds hard 2 MiB maximum")
        detected_secret = secret_summary(raw)
        if detected_secret is not None:
            raise PlanError(
                f"plan contains high-confidence secret material ({detected_secret})"
            )
        plan = json.loads(raw.decode("utf-8"))
    except PlanError:
        raise
    except (OSError, UnicodeError, json.JSONDecodeError, TypeError, ValueError) as error:
        raise PlanError(f"cannot read plan {args.plan}: {error}") from error
    if args.expected_hash:
        require(isinstance(plan, dict), "plan root must be an object")
        try:
            print(plan_hash(plan))
        except (TypeError, ValueError) as error:
            raise PlanError(f"cannot hash noncanonical JSON: {error}") from error
        return 0
    try:
        result = validate(
            plan,
            root,
            raw_size=len(raw),
            allow_greenfield_postcondition=args.native_greenfield_postcondition,
        )
    except ValueError as error:
        raise PlanError(f"plan text is invalid: {error}") from error
    json.dump(result, sys.stdout, ensure_ascii=True, sort_keys=True)
    sys.stdout.write("\n")
    return 0


if __name__ == "__main__":
    try:
        raise SystemExit(main())
    except PlanError as error:
        print(f"validate_plan.py: {error}", file=sys.stderr)
        raise SystemExit(2) from error
