#!/usr/bin/env python3
"""Strict, reconstructible repo-sync-v1 manifest parsing and Git verification."""

from __future__ import annotations

import hashlib
import json
import os
from pathlib import Path, PurePosixPath
import re
import stat
import subprocess

from plan_contract import (
    BOOTSTRAP_EDGE_KINDS,
    HARD_MAXIMA,
    NAMESPACE,
    SCHEMA_VERSION,
    canonical_sources,
    canonical_unit_number,
    edge_assertion_hash,
    has_forbidden_controls,
    materialization_hash,
    node_content_hash,
    normalize_text,
    render_body,
)
from secret_scan import summary as secret_summary


HEX40_OR_64 = re.compile(r"^(?:[0-9a-f]{40}|[0-9a-f]{64})$")
HEX64 = re.compile(r"^[0-9a-f]{64}$")
ULID = re.compile(r"^[0-9A-HJKMNP-TV-Z]{26}$")
KEY = re.compile(r"^[a-z0-9][a-z0-9._:/#-]{0,255}$")
EDGE_KEY = re.compile(r"^[a-z0-9][a-z0-9._:/#>-]{0,511}$")
SPAN = re.compile(r"^[1-9][0-9]*:[1-9][0-9]*$")
CANONICAL_MANIFEST_PATH = ".mneme/bootstrap/manifest.json"


class ManifestContractError(RuntimeError):
    pass


def require(condition: bool, message: str) -> None:
    if not condition:
        raise ManifestContractError(message)


def _lstat(path: Path, context: str) -> os.stat_result | None:
    try:
        return path.lstat()
    except FileNotFoundError:
        return None
    except OSError as error:
        raise ManifestContractError(f"cannot inspect {context} {path}: {error}") from error


def _require_real_directory(path: Path, context: str) -> None:
    metadata = _lstat(path, context)
    require(
        metadata is not None
        and stat.S_ISDIR(metadata.st_mode)
        and not stat.S_ISLNK(metadata.st_mode),
        f"{context} {path} is not a real directory",
    )


def _require_regular_file(path: Path, context: str) -> None:
    metadata = _lstat(path, context)
    require(
        metadata is not None
        and stat.S_ISREG(metadata.st_mode)
        and not stat.S_ISLNK(metadata.st_mode)
        and metadata.st_nlink == 1,
        f"{context} {path} is not a single-linked real regular file",
    )


def resolve_manifest_authority(
    root: Path,
    requested_path: Path,
    logical_relative: str,
    *,
    allow_unactivated_catalog: bool = False,
) -> Path:
    """Resolve the one ownership manifest without silently changing modes.

    The public/logical name remains `.mneme/bootstrap/manifest.json`. A native
    generation stores its immutable creation manifest beside its database and
    selects both through `.mneme/current`. Custom sidecar names remain literal
    only before a generation catalog exists; they cannot bypass an activated or
    interrupted native graph and make it appear brownfield.
    """

    root = root.resolve(strict=True)
    requested_path = Path(os.path.abspath(requested_path))
    mneme = root / ".mneme"
    mneme_metadata = _lstat(mneme, "mneme directory")
    if mneme_metadata is not None:
        require(
            stat.S_ISDIR(mneme_metadata.st_mode)
            and not stat.S_ISLNK(mneme_metadata.st_mode),
            f"mneme directory {mneme} is not a real directory",
        )

    current = mneme / "current"
    generations = mneme / "generations"
    current_metadata = _lstat(current, "mneme activation")
    generations_metadata = _lstat(generations, "mneme generation catalog")

    if current_metadata is None:
        if generations_metadata is not None:
            require(
                stat.S_ISDIR(generations_metadata.st_mode)
                and not stat.S_ISLNK(generations_metadata.st_mode),
                f"mneme generation catalog {generations} is not a real directory",
            )
            require(
                allow_unactivated_catalog,
                f"mneme generation catalog {generations} exists without an activation; "
                "run exact native bootstrap recovery",
            )
            expected = root / CANONICAL_MANIFEST_PATH
            require(
                logical_relative == CANONICAL_MANIFEST_PATH
                and requested_path == expected,
                "native greenfield recovery requires the canonical absent manifest base",
            )
            require(
                _lstat(expected, "conventional manifest") is None,
                "native greenfield recovery found a competing conventional manifest",
            )
            return expected
        if logical_relative == CANONICAL_MANIFEST_PATH:
            expected = root / CANONICAL_MANIFEST_PATH
            require(
                requested_path == expected,
                "canonical manifest path does not resolve to its lexical project location",
            )
            conventional_metadata = _lstat(expected, "conventional manifest")
            if conventional_metadata is not None:
                _require_real_directory(expected.parent, "bootstrap sidecar directory")
                _require_regular_file(expected, "conventional manifest")
            return expected
        return requested_path

    require(
        stat.S_ISLNK(current_metadata.st_mode),
        f"mneme activation {current} is not a symlink",
    )
    require(
        logical_relative == CANONICAL_MANIFEST_PATH,
        "an activated mneme graph requires the canonical manifest path "
        f"{CANONICAL_MANIFEST_PATH}",
    )
    expected_conventional = root / CANONICAL_MANIFEST_PATH
    require(
        requested_path == expected_conventional,
        "canonical manifest path does not resolve to its lexical project location",
    )
    require(
        _lstat(expected_conventional, "conventional manifest") is None,
        "ambiguous mneme ownership: conventional and activated manifests both exist",
    )
    for legacy in (mneme / "memory.db", mneme / "memory.json", mneme / "memory.bodies"):
        require(
            _lstat(legacy, "conventional mneme store artifact") is None,
            f"ambiguous mneme store: {legacy} coexists with activation {current}",
        )

    try:
        target_text = os.readlink(current)
    except OSError as error:
        raise ManifestContractError(f"cannot read mneme activation {current}: {error}") from error
    target = PurePosixPath(target_text)
    require(
        not target.is_absolute()
        and len(target.parts) == 2
        and target.parts[0] == "generations"
        and ULID.fullmatch(target.parts[1]) is not None
        and target.as_posix() == target_text,
        f"mneme activation {current} must target relative generations/<ULID>",
    )

    _require_real_directory(generations, "mneme generation catalog")
    generation = generations / target.parts[1]
    _require_real_directory(generation, "activated mneme generation")
    database = generation / "memory.db"
    _require_regular_file(database, "activated mneme database")
    bootstrap = generation / "bootstrap"
    _require_real_directory(bootstrap, "activated bootstrap directory")
    manifest = bootstrap / "manifest.json"
    _require_regular_file(manifest, "activated manifest")

    canonical_generations = generations.resolve(strict=True)
    canonical_generation = generation.resolve(strict=True)
    canonical_database = database.resolve(strict=True)
    canonical_manifest = manifest.resolve(strict=True)
    require(
        canonical_generation.parent == canonical_generations,
        "activated mneme generation escapes its catalog",
    )
    require(
        canonical_database.parent == canonical_generation
        and canonical_database.name == "memory.db",
        "activated mneme database escapes its generation",
    )
    require(
        canonical_manifest.parent == canonical_generation / "bootstrap"
        and canonical_manifest.name == "manifest.json",
        "activated manifest escapes its generation",
    )
    try:
        final_target = os.readlink(current)
    except OSError as error:
        raise ManifestContractError(
            f"mneme activation {current} changed while resolving: {error}"
        ) from error
    require(final_target == target_text, f"mneme activation {current} changed while resolving")
    return canonical_manifest


def safe_path(value: object, context: str) -> str:
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
        raise ManifestContractError(f"{context}: path is not valid UTF-8") from error
    parsed = PurePosixPath(value)
    require(not parsed.is_absolute() and ".." not in parsed.parts, f"{context}: path is unsafe")
    require(parsed.as_posix() == value and value != ".", f"{context}: path is noncanonical")
    return value


def utf8_bytes(value: str, context: str) -> bytes:
    try:
        return value.encode("utf-8", "strict")
    except UnicodeEncodeError as error:
        raise ManifestContractError(f"{context}: text is not valid UTF-8") from error


def unit_number(value: object, context: str) -> float:
    try:
        number = canonical_unit_number(value, context)
    except ValueError as error:
        raise ManifestContractError(str(error)) from error
    require(
        isinstance(value, float) and repr(value) != "-0.0",
        f"{context}: number is not in canonical float form",
    )
    return number


def validate_evidence(value: object, context: str) -> list[dict]:
    require(isinstance(value, list) and bool(value), f"{context}: evidence must be nonempty")
    require(
        len(value) <= HARD_MAXIMA["sources_per_record"],
        f"{context}: evidence exceeds hard source-per-record maximum",
    )
    for index, source in enumerate(value):
        item = f"{context}[{index}]"
        require(isinstance(source, dict), f"{item}: evidence must be an object")
        require(
            set(source) == {"path", "blob", "sha256", "span"},
            f"{item}: evidence fields must be exact",
        )
        safe_path(source.get("path"), item)
        require(
            isinstance(source.get("blob"), str)
            and HEX40_OR_64.fullmatch(source["blob"]) is not None,
            f"{item}: invalid blob",
        )
        require(
            isinstance(source.get("sha256"), str)
            and HEX64.fullmatch(source["sha256"]) is not None,
            f"{item}: invalid sha256",
        )
        require(
            isinstance(source.get("span"), str) and SPAN.fullmatch(source["span"]) is not None,
            f"{item}: inclusive span is required",
        )
        start, end = map(int, source["span"].split(":"))
        require(start <= end, f"{item}: span is reversed")
    require(value == canonical_sources(value), f"{context}: evidence must be sorted and canonical")
    require(
        len({(item["path"], item["blob"], item["sha256"], item["span"]) for item in value})
        == len(value),
        f"{context}: duplicate evidence",
    )
    return value


def validate_node_version(value: object, context: str, *, current: bool) -> dict:
    require(isinstance(value, dict), f"{context}: node version must be an object")
    expected = {
        "node_id",
        "content_hash",
        "materialization_hash",
        "summary",
        "claim",
        "tags",
        "status",
        "stability",
        "confidence",
        "body_source_commit",
        "evidence_commit",
        "evidence",
        "state",
    }
    require(set(value) == expected, f"{context}: node version fields must be exact")
    require(
        isinstance(value.get("node_id"), str) and ULID.fullmatch(value["node_id"]) is not None,
        f"{context}: invalid node_id",
    )
    for field in ("content_hash", "materialization_hash"):
        require(
            isinstance(value.get(field), str) and HEX64.fullmatch(value[field]) is not None,
            f"{context}: invalid {field}",
        )
    for field in ("body_source_commit", "evidence_commit"):
        require(
            isinstance(value.get(field), str)
            and HEX40_OR_64.fullmatch(value[field]) is not None,
            f"{context}: invalid {field}",
        )
    for field in ("summary", "claim"):
        try:
            normalized = normalize_text(value.get(field)) if isinstance(value.get(field), str) else None
        except ValueError as error:
            raise ManifestContractError(f"{context}: {field} contains a forbidden control character") from error
        require(
            isinstance(value.get(field), str)
            and bool(value[field])
            and value[field] == normalized,
            f"{context}: {field} is blank or noncanonical",
        )
        utf8_bytes(value[field], f"{context}.{field}")
    require(
        len(utf8_bytes(value["summary"], f"{context}.summary"))
        <= HARD_MAXIMA["summary_bytes"],
        f"{context}: summary exceeds hard maximum",
    )
    tags = value.get("tags")
    require(
        isinstance(tags, list) and all(isinstance(tag, str) and bool(tag) for tag in tags),
        f"{context}: invalid tags",
    )
    require(tags == sorted(set(tags)) and NAMESPACE in tags, f"{context}: tags are noncanonical")
    require(len(tags) <= HARD_MAXIMA["tags_per_node"], f"{context}: too many tags")
    require(
        all(
            not has_forbidden_controls(tag)
            and len(utf8_bytes(tag, f"{context}.tags")) <= HARD_MAXIMA["tag_bytes"]
            for tag in tags
        ),
        f"{context}: tag has a forbidden control character or exceeds hard maximum",
    )
    require(value.get("status") in {"active", "candidate"}, f"{context}: invalid status")
    unit_number(value.get("stability"), f"{context}.stability")
    unit_number(value.get("confidence"), f"{context}.confidence")
    evidence = validate_evidence(value.get("evidence"), f"{context}.evidence")
    expected_state = "current" if current else {"superseded", "retired"}
    require(
        value.get("state") == expected_state
        if current
        else value.get("state") in expected_state,
        f"{context}: invalid lifecycle state",
    )
    return {**value, "evidence": evidence}


def validate_edge_version(value: object, context: str, *, current: bool) -> dict:
    require(isinstance(value, dict), f"{context}: edge version must be an object")
    expected = {
        "from_key",
        "to_key",
        "from_node_id",
        "to_node_id",
        "kind",
        "weight",
        "assertion_hash",
        "evidence_commit",
        "evidence",
        "state",
    }
    require(set(value) == expected, f"{context}: edge version fields must be exact")
    for field in ("from_key", "to_key"):
        require(
            isinstance(value.get(field), str) and KEY.fullmatch(value[field]) is not None,
            f"{context}: invalid {field}",
        )
    require(value["from_key"] != value["to_key"], f"{context}: self-loop edge is forbidden")
    for field in ("from_node_id", "to_node_id"):
        require(
            isinstance(value.get(field), str) and ULID.fullmatch(value[field]) is not None,
            f"{context}: invalid {field}",
        )
    require(value.get("kind") in BOOTSTRAP_EDGE_KINDS, f"{context}: invalid edge kind")
    unit_number(value.get("weight"), f"{context}.weight")
    require(
        isinstance(value.get("assertion_hash"), str)
        and HEX64.fullmatch(value["assertion_hash"]) is not None,
        f"{context}: invalid assertion_hash",
    )
    require(
        isinstance(value.get("evidence_commit"), str)
        and HEX40_OR_64.fullmatch(value["evidence_commit"]) is not None,
        f"{context}: invalid evidence_commit",
    )
    evidence = validate_evidence(value.get("evidence"), f"{context}.evidence")
    expected_state = "current" if current else {"replaced", "retired"}
    require(
        value.get("state") == expected_state
        if current
        else value.get("state") in expected_state,
        f"{context}: invalid lifecycle state",
    )
    return {**value, "evidence": evidence}


def parse_manifest(path: Path, root: Path | None = None) -> tuple[str, dict]:
    if not path.exists():
        return "absent", {}
    try:
        raw = path.read_bytes()
        require(
            len(raw) <= HARD_MAXIMA["manifest_bytes"],
            "manifest exceeds hard 2 MiB maximum",
        )
        value = json.loads(raw.decode("utf-8"))
    except (OSError, UnicodeError, json.JSONDecodeError) as error:
        raise ManifestContractError(f"cannot read manifest {path}: {error}") from error
    detected_secret = secret_summary(raw)
    require(
        detected_secret is None,
        f"manifest contains high-confidence secret material ({detected_secret})",
    )
    require(isinstance(value, dict), "manifest root must be an object")
    expected_fields = {
        "schema_version",
        "namespace",
        "db_id",
        "generation",
        "repo",
        "files",
        "nodes",
        "edges",
        "applied_plan_hash",
    }
    require(set(value) == expected_fields, "manifest fields do not match repo-sync-v1")
    require(value.get("schema_version") == SCHEMA_VERSION, "unsupported manifest schema")
    require(value.get("namespace") == NAMESPACE, "unsupported manifest namespace")
    require(
        isinstance(value.get("db_id"), str) and ULID.fullmatch(value["db_id"]) is not None,
        "manifest db_id is invalid",
    )
    generation = value.get("generation")
    require(
        isinstance(generation, int) and not isinstance(generation, bool) and generation >= 1,
        "manifest generation must be a positive integer",
    )
    repo = value.get("repo")
    require(
        isinstance(repo, dict)
        and set(repo) == {"head", "tree", "object_format", "dirty_digest"},
        "manifest repo fields are invalid",
    )
    for field in ("head", "tree"):
        require(
            isinstance(repo.get(field), str) and HEX40_OR_64.fullmatch(repo[field]) is not None,
            f"manifest repo.{field} is invalid",
        )
    require(repo.get("object_format") in {"sha1", "sha256"}, "invalid Git object format")
    require(repo.get("dirty_digest") is None, "manifest must identify a committed source cut")

    files = value.get("files")
    require(isinstance(files, dict), "manifest files must be an object")
    for source_path, record in files.items():
        safe_path(source_path, "manifest file")
        require(
            isinstance(record, dict)
            and set(record) == {"blob", "sha256"}
            and isinstance(record.get("blob"), str)
            and HEX40_OR_64.fullmatch(record["blob"]) is not None
            and isinstance(record.get("sha256"), str)
            and HEX64.fullmatch(record["sha256"]) is not None,
            f"invalid manifest file record for {source_path!r}",
        )

    nodes = value.get("nodes")
    require(isinstance(nodes, dict), "manifest nodes must be an object")
    require(len(nodes) <= HARD_MAXIMA["nodes"], "manifest node catalog exceeds hard maximum")
    owned_node_ids: set[str] = set()
    current_file_records: dict[str, tuple[str, str]] = {}
    current_core_count = 0
    for key, record in nodes.items():
        require(isinstance(key, str) and KEY.fullmatch(key) is not None, "invalid manifest node key")
        require(
            isinstance(record, dict) and set(record) == {"current", "history"},
            f"manifest node {key!r} fields are invalid",
        )
        current = record.get("current")
        history = record.get("history")
        require(isinstance(history, list), f"manifest node {key!r} history must be an array")
        versions: list[dict] = []
        if current is not None:
            current = validate_node_version(current, f"manifest node {key!r}.current", current=True)
            versions.append(current)
            if "core" in current["tags"]:
                require(current["status"] == "active", f"manifest node {key!r} core is not active")
                current_core_count += 1
            require(
                current["evidence_commit"] == repo["head"],
                f"manifest node {key!r} current evidence cut differs from manifest repo",
            )
            for source in current["evidence"]:
                current_file_records[source["path"]] = (source["blob"], source["sha256"])
        for index, item in enumerate(history):
            versions.append(
                validate_node_version(
                    item, f"manifest node {key!r}.history[{index}]", current=False
                )
            )
        require(current is not None or bool(history), f"manifest node {key!r} has no versions")
        require(
            history == sorted(history, key=lambda item: item["node_id"]),
            f"manifest node {key!r} history must be sorted by node_id",
        )
        for version in versions:
            require(version["node_id"] not in owned_node_ids, "duplicate owned node_id in manifest")
            owned_node_ids.add(version["node_id"])
            expected_content = node_content_hash(
                key=key,
                summary=version["summary"],
                claim=version["claim"],
                sources=version["evidence"],
            )
            require(
                version["content_hash"] == expected_content,
                f"manifest node {key!r} content hash mismatch",
            )
            body = render_body(
                key=key,
                content_hash=expected_content,
                source_commit=version["body_source_commit"],
                claim=version["claim"],
            )
            if "core" in version["tags"]:
                require(
                    len(body.encode("utf-8")) <= HARD_MAXIMA["core_body_bytes"],
                    f"manifest node {key!r} core body exceeds hard maximum",
                )
            expected_materialization = materialization_hash(
                content_hash=expected_content,
                body=body,
                tags=version["tags"],
                status=version["status"],
                stability=version["stability"],
                confidence=version["confidence"],
            )
            require(
                version["materialization_hash"] == expected_materialization,
                f"manifest node {key!r} materialization hash mismatch",
            )

    edges = value.get("edges")
    require(isinstance(edges, dict), "manifest edges must be an object")
    require(current_core_count <= 1, "manifest has more than one current core node")
    require(len(edges) <= HARD_MAXIMA["edges"], "manifest edge catalog exceeds hard maximum")
    current_pairs: set[tuple[str, str]] = set()
    current_associative_pairs: set[tuple[str, str]] = set()
    for key, record in edges.items():
        require(
            isinstance(key, str) and EDGE_KEY.fullmatch(key) is not None,
            "invalid manifest edge key",
        )
        require(
            isinstance(record, dict) and set(record) == {"current", "history"},
            f"manifest edge {key!r} fields are invalid",
        )
        current = record.get("current")
        history = record.get("history")
        require(isinstance(history, list), f"manifest edge {key!r} history must be an array")
        versions: list[dict] = []
        if current is not None:
            current = validate_edge_version(current, f"manifest edge {key!r}.current", current=True)
            versions.append(current)
            require(
                current["evidence_commit"] == repo["head"],
                f"manifest edge {key!r} current evidence cut differs from manifest repo",
            )
            pair = (current["from_key"], current["to_key"])
            require(pair not in current_pairs, f"duplicate current edge endpoint pair {pair!r}")
            unordered = tuple(sorted(pair))
            require(
                unordered not in current_associative_pairs,
                f"current edge conflicts with associative pair {unordered!r}",
            )
            if current["kind"] == "associative":
                require(
                    (pair[1], pair[0]) not in current_pairs,
                    f"duplicate reverse associative edge pair {unordered!r}",
                )
                current_associative_pairs.add(unordered)
            current_pairs.add(pair)
            for source in current["evidence"]:
                current_file_records[source["path"]] = (source["blob"], source["sha256"])
        for index, item in enumerate(history):
            versions.append(
                validate_edge_version(
                    item, f"manifest edge {key!r}.history[{index}]", current=False
                )
            )
        require(current is not None or bool(history), f"manifest edge {key!r} has no versions")
        require(
            history
            == sorted(
                history,
                key=lambda item: (
                    item["from_node_id"],
                    item["to_node_id"],
                    item["assertion_hash"],
                ),
            ),
            f"manifest edge {key!r} history is not canonical",
        )
        for version in versions:
            require(version["from_key"] in nodes and version["to_key"] in nodes, "unknown edge key")
            require(
                version["from_node_id"] in owned_node_ids
                and version["to_node_id"] in owned_node_ids,
                f"manifest edge {key!r} references an unowned physical endpoint",
            )
            expected_assertion = edge_assertion_hash(
                key=key,
                from_key=version["from_key"],
                to_key=version["to_key"],
                kind=version["kind"],
                weight=version["weight"],
                sources=version["evidence"],
            )
            require(
                version["assertion_hash"] == expected_assertion,
                f"manifest edge {key!r} assertion hash mismatch",
            )
        if current is not None:
            require(
                nodes[current["from_key"]]["current"] is not None
                and nodes[current["to_key"]]["current"] is not None,
                f"manifest edge {key!r} points at a node without a current version",
            )
            require(
                current["from_node_id"] == nodes[current["from_key"]]["current"]["node_id"]
                and current["to_node_id"] == nodes[current["to_key"]]["current"]["node_id"],
                f"manifest edge {key!r} does not target current physical node versions",
            )

    expected_files = {
        path: {"blob": blob, "sha256": sha256}
        for path, (blob, sha256) in sorted(current_file_records.items())
    }
    require(len(files) <= HARD_MAXIMA["sources"], "manifest file catalog exceeds hard maximum")
    require(files == expected_files, "manifest files must exactly cover current managed evidence")
    require(
        isinstance(value.get("applied_plan_hash"), str)
        and HEX64.fullmatch(value["applied_plan_hash"]) is not None,
        "manifest applied_plan_hash is invalid",
    )
    if root is not None:
        verify_manifest_git(root, value)
    return "valid", value


def run_git(root: Path, *args: str, allow_one: bool = False) -> bytes:
    result = subprocess.run(
        ["git", "--no-replace-objects", "-C", str(root), *args],
        stdout=subprocess.PIPE,
        stderr=subprocess.PIPE,
        check=False,
        env={
            **os.environ,
            "GIT_NO_REPLACE_OBJECTS": "1",
            "GIT_GRAFT_FILE": os.devnull,
        },
    )
    allowed = {0, 1} if allow_one else {0}
    if result.returncode not in allowed:
        detail = result.stderr.decode("utf-8", "replace").strip()
        raise ManifestContractError(f"git {' '.join(args)} failed: {detail}")
    return result.stdout


def verify_commit(root: Path, commit: str, current_head: str, cache: set[str]) -> None:
    if commit in cache:
        return
    resolved = run_git(root, "rev-parse", "--verify", f"{commit}^{{commit}}").decode().strip()
    require(resolved == commit, f"historical evidence commit {commit} is not exact")
    ancestor = subprocess.run(
        ["git", "--no-replace-objects", "-C", str(root), "merge-base", "--is-ancestor", commit, current_head],
        stdout=subprocess.PIPE,
        stderr=subprocess.PIPE,
        check=False,
        env={
            **os.environ,
            "GIT_NO_REPLACE_OBJECTS": "1",
            "GIT_GRAFT_FILE": os.devnull,
        },
    )
    require(ancestor.returncode == 0, f"historical evidence commit {commit} is not an ancestor")
    cache.add(commit)


def verify_manifest_git(root: Path, manifest: dict) -> None:
    repo = manifest["repo"]
    head = repo["head"]
    resolved = run_git(root, "rev-parse", "--verify", f"{head}^{{commit}}").decode().strip()
    require(resolved == head, "manifest repo.head is not an exact commit")
    tree = run_git(root, "rev-parse", f"{head}^{{tree}}").decode().strip()
    require(tree == repo["tree"], "manifest repo.tree does not match repo.head")
    object_format = run_git(root, "rev-parse", "--show-object-format").decode().strip()
    require(object_format == repo["object_format"], "manifest Git object format mismatch")

    commit_cache = {head}
    blob_cache: dict[str, bytes] = {}

    def verify_source(commit: str, source: dict, context: str) -> None:
        verify_commit(root, commit, head, commit_cache)
        actual_blob = run_git(root, "rev-parse", f"{commit}:{source['path']}").decode().strip()
        require(actual_blob == source["blob"], f"{context}: blob does not exist at historical cut")
        content = blob_cache.get(actual_blob)
        if content is None:
            content = run_git(root, "cat-file", "blob", actual_blob)
            blob_cache[actual_blob] = content
        require(
            len(content) <= HARD_MAXIMA["source_bytes"],
            f"{context}: historical evidence exceeds hard 1 MiB source maximum",
        )
        require(b"\0" not in content, f"{context}: historical evidence is binary")
        require(
            hashlib.sha256(content).hexdigest() == source["sha256"],
            f"{context}: sha256 does not match historical blob bytes",
        )
        try:
            content.decode("utf-8", "strict")
        except UnicodeDecodeError as error:
            raise ManifestContractError(f"{context}: historical evidence is not UTF-8") from error
        line_count = content.count(b"\n") + int(bool(content) and not content.endswith(b"\n"))
        _start, end = map(int, source["span"].split(":"))
        require(end <= line_count, f"{context}: historical evidence span exceeds blob")

    for path, record in manifest["files"].items():
        verify_source(
            head,
            {"path": path, "blob": record["blob"], "sha256": record["sha256"], "span": "1:1"},
            f"manifest file {path!r}",
        )
    for key, record in manifest["nodes"].items():
        versions = ([record["current"]] if record["current"] is not None else []) + record["history"]
        for version in versions:
            verify_commit(root, version["body_source_commit"], head, commit_cache)
            for source in version["evidence"]:
                verify_source(version["evidence_commit"], source, f"manifest node {key!r}")
    for key, record in manifest["edges"].items():
        versions = ([record["current"]] if record["current"] is not None else []) + record["history"]
        for version in versions:
            for source in version["evidence"]:
                verify_source(version["evidence_commit"], source, f"manifest edge {key!r}")
