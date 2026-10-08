#!/usr/bin/env python3
"""Emit a deterministic, read-only inventory of a committed Git tree."""

from __future__ import annotations

import argparse
import hashlib
import io
import json
import os
from pathlib import Path, PurePosixPath
import subprocess
import sys
import tempfile
from typing import BinaryIO, Iterable

from manifest_contract import (
    ManifestContractError,
    parse_manifest as parse_manifest_contract,
    resolve_manifest_authority,
)
from plan_contract import (
    HARD_MAXIMA,
    canonical_bytes,
    canonical_sha256,
    has_forbidden_controls,
)
from secret_scan import findings as secret_findings, summary as secret_summary


SCHEMA_VERSION = 2
NAMESPACE = "repo-sync-v1"
DEFAULT_MAX_BYTES = 1_048_576
TEXT_EXTENSIONS = {
    ".c", ".cc", ".cfg", ".clj", ".cpp", ".cs", ".css", ".ex", ".exs",
    ".go", ".graphql", ".h", ".hpp", ".html", ".java", ".js", ".json",
    ".jsx", ".kt", ".kts", ".lua", ".md", ".mdx", ".nix", ".php",
    ".proto", ".py", ".rb", ".rs", ".rst", ".scala", ".sh", ".sql",
    ".swift", ".toml", ".ts", ".tsx", ".txt", ".xml", ".yaml", ".yml",
    ".zig",
}
MANIFEST_NAMES = {
    "Cargo.toml", "package.json", "pyproject.toml", "requirements.txt", "go.mod",
    "Gemfile", "pom.xml", "build.gradle", "settings.gradle", "flake.nix",
    "docker-compose.yml", "docker-compose.yaml", "Dockerfile", "Containerfile",
    "Makefile", "Justfile",
}
LOCKFILE_NAMES = {
    "Cargo.lock", "package-lock.json", "pnpm-lock.yaml", "yarn.lock", "poetry.lock",
    "go.sum", "Gemfile.lock",
}
POLICY_NAMES = {
    ".dockerignore", ".gitignore", "AGENTS.md", "CLAUDE.md", "CONTRIBUTING.md",
    "SECURITY.md", "CODE_OF_CONDUCT.md",
}
SKIP_PARTS = {
    ".git", ".mneme", ".next", ".venv", "__pycache__", "build", "coverage", "dist",
    "node_modules", "target", "vendor",
}
SECRET_NAMES = {
    ".env", ".env.local", ".env.production", "credentials", "credentials.json",
    "id_rsa", "id_ed25519", "secrets.yml", "secrets.yaml", "secrets.toml",
    ".netrc", ".npmrc", ".pypirc", "auth.json",
}
SECRET_EXTENSIONS = {".key", ".p12", ".pfx", ".pem", ".pkcs12"}


class InventoryError(RuntimeError):
    pass


def git(root: Path, *args: str, input_bytes: bytes | None = None) -> bytes:
    env = dict(os.environ)
    env["GIT_NO_REPLACE_OBJECTS"] = "1"
    env["GIT_GRAFT_FILE"] = os.devnull
    completed = subprocess.run(
        ["git", "--no-replace-objects", "-C", str(root), *args],
        input=input_bytes,
        stdout=subprocess.PIPE,
        stderr=subprocess.PIPE,
        check=False,
        env=env,
    )
    if completed.returncode != 0:
        detail = completed.stderr.decode("utf-8", "replace").strip()
        raise InventoryError(f"git {' '.join(args)} failed: {detail}")
    return completed.stdout


def ignored_paths(_root: Path, _paths: Iterable[str]) -> set[str]:
    """Return no ignore matches for an already committed tree.

    `git ls-tree` contains tracked objects, so worktree ignore rules are irrelevant.
    Consulting `git check-ignore` here would let an uncommitted `.gitignore` change
    alter an otherwise commit-frozen inventory.
    """

    return set()


def parse_manifest(path: Path, root: Path | None = None) -> tuple[str, dict]:
    try:
        return parse_manifest_contract(path, root)
    except ManifestContractError as error:
        raise InventoryError(str(error)) from error


def resolve_sidecar_path(root: Path, value: str, context: str) -> tuple[Path, str]:
    candidate = Path(value)
    if not candidate.is_absolute():
        candidate = root / candidate
    resolved = candidate.resolve(strict=False)
    if resolved == root or root not in resolved.parents:
        raise InventoryError(f"{context} must remain inside the repository root")
    relative = PurePosixPath(os.path.relpath(resolved, root)).as_posix()
    parts = PurePosixPath(relative).parts
    if len(parts) < 3 or parts[:2] != (".mneme", "bootstrap"):
        raise InventoryError(f"{context} must be under .mneme/bootstrap")
    if not relative.endswith(".json"):
        raise InventoryError(f"{context} must be a JSON sidecar")
    return resolved, relative


def tree_records(root: Path, revision: str) -> list[dict]:
    raw = git(root, "ls-tree", "-r", "-z", "-l", "--full-tree", revision)
    records: list[dict] = []
    for item in raw.split(b"\0"):
        if not item:
            continue
        try:
            metadata, raw_path = item.split(b"\t", 1)
            mode, kind, oid, size = metadata.split(b" ", 3)
        except ValueError as error:
            raise InventoryError("unexpected git ls-tree record") from error
        try:
            path = raw_path.decode("utf-8", "strict")
        except UnicodeDecodeError as error:
            raise InventoryError(
                "repo-sync-v1 cannot represent a non-UTF-8 Git path"
            ) from error
        if any(
            ord(character) < 0x20 or 0x7F <= ord(character) <= 0x9F
            for character in path
        ):
            raise InventoryError(
                "repo-sync-v1 cannot represent a Git path containing control characters"
            )
        records.append(
            {
                "mode": mode.decode("ascii"),
                "type": kind.decode("ascii"),
                "oid": oid.decode("ascii"),
                "bytes": None if size == b"-" else int(size),
                "path": path,
            }
        )
    return records


def exclusion_reason(record: dict, max_bytes: int, ignored: set[str]) -> str | None:
    path = PurePosixPath(record["path"])
    parts = set(path.parts)
    lower_name = path.name.lower()
    if record["path"] in ignored and path.name != ".gitignore":
        return "gitignored"
    if record["type"] != "blob" or record["mode"] == "160000":
        return "submodule_or_non_blob"
    if record["mode"] == "120000":
        return "symlink"
    if parts & SKIP_PARTS:
        return "generated_or_private_tree"
    if (
        lower_name in SECRET_NAMES
        or lower_name.startswith(".env.")
        or path.suffix.lower() in SECRET_EXTENSIONS
    ):
        return "secret_like_path"
    size = record["bytes"]
    if size is None or size > max_bytes:
        return "too_large"
    if not is_text_candidate(path):
        return "non_text_extension"
    return None


def content_exclusion_reason(record: dict, content: bytes) -> str | None:
    if b"\0" in content:
        return "binary_content"
    if content.startswith(b"version https://git-lfs.github.com/spec/v1"):
        return "git_lfs_pointer"
    try:
        text = content.decode("utf-8", "strict")
    except UnicodeDecodeError:
        return "non_utf8_content"
    if has_forbidden_controls(text, multiline=True):
        return "control_character_content"
    if secret_findings(content):
        return "secret_like_content"
    path = PurePosixPath(record["path"])
    if (
        not path.suffix
        and path.name not in MANIFEST_NAMES
        and path.name not in LOCKFILE_NAMES
        and path.name not in POLICY_NAMES
        and not path.name.upper().startswith(("README", "LICENSE", "NOTICE", "CHANGELOG"))
        and not content.startswith(b"#!")
    ):
        return "extensionless_non_shebang"
    return None


def is_text_candidate(path: PurePosixPath) -> bool:
    name = path.name
    upper = name.upper()
    if path.suffix.lower() in TEXT_EXTENSIONS:
        return True
    if name in MANIFEST_NAMES or name in LOCKFILE_NAMES or name in POLICY_NAMES:
        return True
    if not path.suffix:
        return True
    return upper.startswith(("README", "LICENSE", "NOTICE", "CHANGELOG"))


def classify(path_text: str) -> tuple[str, int]:
    path = PurePosixPath(path_text)
    name = path.name
    lower = path_text.lower()
    upper = name.upper()
    if len(path.parts) >= 2 and path.parts[0].lower() == "benchmarks" and path.parts[1].lower() == "results":
        return "benchmark_artifact", 18
    if upper.startswith("README"):
        return "overview", 100
    if name in POLICY_NAMES or "security" in lower or "threat" in lower:
        return "policy", 96
    if (
        "adr" in {part.lower() for part in path.parts}
        or name.lower().startswith("adr-")
        or any(token in lower for token in ("architecture", "design", "decision", "rfc", "spec"))
    ):
        return "design", 94
    if name in MANIFEST_NAMES:
        return "manifest", 88
    if name in LOCKFILE_NAMES:
        return "lockfile", 24
    if path.parts and path.parts[0].lower() in {"docs", "doc"}:
        return "documentation", 82
    if any(part.lower() in {"migration", "migrations", "schema", "schemas"} for part in path.parts):
        return "schema", 78
    if any(part.lower() in {"test", "tests", "spec", "specs"} for part in path.parts):
        return "test", 62
    if path.stem.lower() in {"main", "lib", "mod", "index", "app"}:
        return "entrypoint", 68
    if path.suffix.lower() in TEXT_EXTENSIONS:
        return "source", 40
    return "other", 20


def read_batch_blobs(root: Path, records: Iterable[dict]) -> dict[str, bytes]:
    unique_records: dict[str, dict] = {}
    for record in records:
        unique_records.setdefault(record["oid"], record)
    records = list(unique_records.values())
    if not records:
        return {}
    raw = git(
        root,
        "cat-file",
        "--batch",
        input_bytes=b"".join(record["oid"].encode("ascii") + b"\n" for record in records),
    )
    stream = io.BytesIO(raw)

    result: dict[str, bytes] = {}
    for expected in records:
        header = stream.readline().rstrip(b"\n")
        fields = header.split(b" ")
        if len(fields) != 3 or fields[1] != b"blob":
            raise InventoryError(f"unexpected git cat-file response for {expected['path']!r}")
        size = int(fields[2])
        content = read_exact(stream, size)
        if stream.read(1) != b"\n":
            raise InventoryError("malformed git cat-file batch delimiter")
        result[expected["oid"]] = content
    if stream.read(1):
        raise InventoryError("unexpected trailing git cat-file batch output")
    return result


def content_batches(records: Iterable[dict]) -> Iterable[list[dict]]:
    """Bound cat-file process count without retaining an unbounded content batch."""

    batch: list[dict] = []
    batch_bytes = 0
    for record in records:
        size = record["bytes"] or 0
        if batch and (len(batch) >= 2_048 or batch_bytes + size > 16_777_216):
            yield batch
            batch = []
            batch_bytes = 0
        batch.append(record)
        batch_bytes += size
    if batch:
        yield batch


def read_exact(stream: BinaryIO, size: int) -> bytes:
    chunks: list[bytes] = []
    remaining = size
    while remaining:
        chunk = stream.read(remaining)
        if not chunk:
            raise InventoryError("truncated git object stream")
        chunks.append(chunk)
        remaining -= len(chunk)
    return b"".join(chunks)


def bounded_selection(
    records: list[dict],
    manifest_files: dict,
    rename_candidate_paths: set[str],
    max_files: int,
) -> tuple[list[dict], list[dict]]:
    ranked = []
    for record in records:
        category, priority = classify(record["path"])
        record = dict(record)
        record["class"] = category
        record["priority"] = priority
        record["managed_source"] = record["path"] in manifest_files
        record["rename_candidate"] = record["path"] in rename_candidate_paths
        ranked.append(record)
    ranked.sort(
        key=lambda item: (
            -int(item["managed_source"]),
            -int(item["rename_candidate"]),
            -item["priority"],
            item["path"],
        )
    )
    managed_count = sum(
        bool(item["managed_source"] or item["rename_candidate"]) for item in ranked
    )
    if managed_count > max_files:
        raise InventoryError(
            f"{managed_count} managed or rename-candidate source files exceed "
            f"--max-files {max_files}; "
            "raise the limit so refresh cannot silently omit owned inputs"
        )
    return ranked[:max_files], ranked[max_files:]


def change_for(record: dict, previous: dict | None) -> str:
    if previous is None:
        return "added"
    if not isinstance(previous, dict):
        return "changed"
    if previous.get("blob") == record["blob"] or previous.get("sha256") == record["sha256"]:
        return "unchanged"
    return "changed"


def inventory_hash(value: dict) -> str:
    payload = dict(value)
    payload.pop("inventory_hash", None)
    return canonical_sha256(payload)


def exclusion_entry(record: dict, reason: str, sha256: str | None = None) -> dict:
    return {
        "path": record["path"],
        "blob": record["oid"],
        "bytes": record["bytes"],
        "mode": record["mode"],
        "type": record["type"],
        "reason": reason,
        "sha256": sha256,
    }


def construct_inventory(
    root: Path,
    manifest_path: Path,
    manifest_relative: str,
    *,
    max_files: int,
    max_bytes: int,
) -> dict:
    if max_files < 1 or max_bytes < 1:
        raise InventoryError("--max-files and --max-bytes must be positive")
    if max_files > 500 or max_bytes > DEFAULT_MAX_BYTES:
        raise InventoryError("inventory limits may lower, but never exceed, repo-sync-v1 policy")

    if git(root, "rev-parse", "--git-dir") == b"":
        raise InventoryError(f"not a Git repository: {root}")
    try:
        resolved_manifest_path = resolve_manifest_authority(
            root, manifest_path, manifest_relative
        )
    except ManifestContractError as error:
        raise InventoryError(str(error)) from error
    manifest_status, manifest = parse_manifest(resolved_manifest_path, root)
    manifest_files = manifest.get("files", {})
    initial_manifest_hash = canonical_sha256(manifest) if manifest else None

    head = git(root, "rev-parse", "--verify", "HEAD^{commit}").decode("ascii").strip()
    tree = git(root, "rev-parse", f"{head}^{{tree}}").decode("ascii").strip()
    object_format = git(root, "rev-parse", "--show-object-format").decode("ascii").strip()

    all_records = tree_records(root, head)
    if len(all_records) > HARD_MAXIMA["inventory_entries"]:
        raise InventoryError(
            f"tree has {len(all_records)} entries; complete inventory maximum is "
            f"{HARD_MAXIMA['inventory_entries']}"
        )
    ignored = ignored_paths(root, (record["path"] for record in all_records))
    candidates: list[dict] = []
    exclusions: list[dict] = []
    for record in all_records:
        reason = exclusion_reason(record, max_bytes, ignored)
        if reason is None:
            candidates.append(record)
        else:
            exclusions.append(exclusion_entry(record, reason))

    scan_bytes = sum(record["bytes"] or 0 for record in candidates)
    if scan_bytes > HARD_MAXIMA["inventory_scan_bytes"]:
        raise InventoryError(
            f"eligible content totals {scan_bytes} bytes; complete eligibility scan maximum is "
            f"{HARD_MAXIMA['inventory_scan_bytes']}"
        )

    # Eligibility is intentionally decided before ranking. Otherwise an attacker
    # can place poisoned high-priority files ahead of safe evidence and consume the
    # selection budget without ever giving the safe files a chance.
    eligible: list[dict] = []
    for batch in content_batches(candidates):
        blobs = read_batch_blobs(root, batch)
        for record in batch:
            content = blobs[record["oid"]]
            digest = hashlib.sha256(content).hexdigest()
            reason = content_exclusion_reason(record, content)
            if reason is not None:
                exclusions.append(exclusion_entry(record, reason, digest))
                continue
            enriched = dict(record)
            enriched["sha256"] = digest
            enriched["lines"] = content.count(b"\n") + int(
                bool(content) and not content.endswith(b"\n")
            )
            eligible.append(enriched)

    current_tree_paths = {record["path"] for record in all_records}
    missing_manifest_paths = {
        path for path in manifest_files if path not in current_tree_paths
    }
    old_paths_by_blob: dict[str, list[str]] = {}
    for old_path in missing_manifest_paths:
        old = manifest_files.get(old_path)
        if isinstance(old, dict) and isinstance(old.get("blob"), str):
            old_paths_by_blob.setdefault(old["blob"], []).append(old_path)
    new_paths_by_blob: dict[str, list[str]] = {}
    for record in candidates:
        if record["path"] not in manifest_files and record["oid"] in old_paths_by_blob:
            new_paths_by_blob.setdefault(record["oid"], []).append(record["path"])

    rename_map: dict[str, str] = {}
    ambiguous_renames: list[dict] = []
    rename_candidate_paths: set[str] = set()
    for blob, old_paths in sorted(old_paths_by_blob.items()):
        new_paths = sorted(new_paths_by_blob.get(blob, []))
        old_paths = sorted(old_paths)
        rename_candidate_paths.update(new_paths)
        if len(old_paths) == 1 and len(new_paths) == 1:
            rename_map[old_paths[0]] = new_paths[0]
        elif new_paths:
            ambiguous_renames.append(
                {"blob": blob, "from_candidates": old_paths, "to_candidates": new_paths}
            )

    if ambiguous_renames:
        details = "; ".join(
            f"{item['from_candidates']} -> {item['to_candidates']}"
            for item in ambiguous_renames
        )
        raise InventoryError(
            "ambiguous managed-source rename; resolve it explicitly before refresh: " + details
        )

    selected, selection_limited = bounded_selection(
        eligible, manifest_files, rename_candidate_paths, max_files
    )
    exclusions.extend(
        exclusion_entry(record, "selection_limit", record["sha256"])
        for record in selection_limited
    )
    files = []
    renamed_from_by_path = {new_path: old_path for old_path, new_path in rename_map.items()}

    for record in selected:
        item = {
            "path": record["path"],
            "blob": record["oid"],
            "sha256": record["sha256"],
            "bytes": record["bytes"],
            "lines": record["lines"],
            "class": record["class"],
            "priority": record["priority"],
            "managed_source": record["managed_source"],
            "rename_candidate": record["rename_candidate"],
        }
        item["change"] = change_for(item, manifest_files.get(record["path"]))
        renamed_from = renamed_from_by_path.get(record["path"])
        if item["change"] == "added" and renamed_from is not None:
            item["change"] = "renamed"
            item["renamed_from"] = renamed_from
        files.append(item)

    paths_by_blob: dict[str, list[str]] = {}
    for item in files:
        paths_by_blob.setdefault(item["blob"], []).append(item["path"])
    duplicate_blob_groups = [
        {"blob": blob, "paths": sorted(paths)}
        for blob, paths in sorted(paths_by_blob.items())
        if len(paths) > 1
    ]
    duplicates_by_path = {
        path: [other for other in group["paths"] if other != path]
        for group in duplicate_blob_groups
        for path in group["paths"]
    }
    for item in files:
        if item["path"] in duplicates_by_path:
            item["duplicate_blob_paths"] = duplicates_by_path[item["path"]]

    deleted = sorted(missing_manifest_paths - rename_map.keys())
    reported_paths = {item["path"] for item in files}
    unavailable_renames = [
        f"{old_path} -> {new_path}"
        for old_path, new_path in sorted(rename_map.items())
        if new_path not in reported_paths
    ]
    if unavailable_renames:
        raise InventoryError(
            "managed rename destination became ineligible; refusing refresh: "
            + ", ".join(unavailable_renames)
        )
    unavailable = sorted(
        path for path in manifest_files if path in current_tree_paths and path not in reported_paths
    )
    if unavailable:
        raise InventoryError(
            "managed sources became ineligible or unavailable; refusing a partial refresh: "
            + ", ".join(unavailable)
        )
    files.sort(key=lambda item: (-int(item["managed_source"]), -item["priority"], item["path"]))
    exclusions.sort(key=lambda item: item["path"])
    selected_paths = {item["path"] for item in files}
    excluded_paths = {item["path"] for item in exclusions}
    tree_paths = {item["path"] for item in all_records}
    if len(excluded_paths) != len(exclusions):
        raise InventoryError("internal error: duplicate exclusion path")
    if selected_paths & excluded_paths or selected_paths | excluded_paths != tree_paths:
        raise InventoryError("internal error: inventory does not partition the complete tree")

    output = {
        "schema_version": SCHEMA_VERSION,
        "namespace": NAMESPACE,
        "mode": "committed_tree",
        "repo": {
            "head": head,
            "tree": tree,
            "object_format": object_format,
        },
        "manifest": {
            "path": manifest_relative,
            "status": manifest_status,
            "generation": manifest.get("generation") if manifest else None,
            "db_id": manifest.get("db_id") if manifest else None,
            "sha256": initial_manifest_hash,
        },
        "limits": {"max_files": max_files, "max_bytes": max_bytes},
        "counts": {
            "tree_entries": len(all_records),
            "eligible_text_files": len(eligible),
            "selected_files": len(files),
            "excluded_files": len(exclusions),
            "selection_limited_files": len(selection_limited),
            "deleted_managed_sources": len(deleted),
        },
        "deleted_managed_sources": deleted,
        "renamed_managed_sources": [
            {
                "from": old_path,
                "to": new_path,
                "blob": manifest_files[old_path]["blob"],
            }
            for old_path, new_path in sorted(rename_map.items())
        ],
        "duplicate_blob_groups": duplicate_blob_groups,
        "files": files,
        "exclusions": exclusions,
        "exclusions_sha256": canonical_sha256(exclusions),
    }
    output["inventory_hash"] = inventory_hash(output)
    encoded = canonical_bytes(output)
    if len(encoded) > HARD_MAXIMA["inventory_bytes"]:
        raise InventoryError(
            f"complete inventory exceeds {HARD_MAXIMA['inventory_bytes']} byte maximum"
        )
    detected_secret = secret_summary(encoded)
    if detected_secret is not None:
        raise InventoryError(
            f"inventory artifact contains high-confidence secret material ({detected_secret})"
        )

    final_head = git(root, "rev-parse", "--verify", "HEAD^{commit}").decode("ascii").strip()
    final_tree = git(root, "rev-parse", f"{head}^{{tree}}").decode("ascii").strip()
    if final_head != head or final_tree != tree:
        raise InventoryError("repository source cut changed while inventory was being built")
    try:
        final_manifest_path = resolve_manifest_authority(
            root, manifest_path, manifest_relative
        )
    except ManifestContractError as error:
        raise InventoryError(str(error)) from error
    if final_manifest_path != resolved_manifest_path:
        raise InventoryError("manifest authority changed while inventory was being built")
    final_manifest_status, final_manifest = parse_manifest(final_manifest_path, root)
    final_manifest_hash = canonical_sha256(final_manifest) if final_manifest else None
    if (
        final_manifest_status != manifest_status
        or final_manifest_hash != initial_manifest_hash
    ):
        raise InventoryError("manifest changed while inventory was being built")
    return output


def load_inventory_artifact(path: Path) -> dict:
    try:
        raw = path.read_bytes()
        if len(raw) > HARD_MAXIMA["inventory_bytes"]:
            raise InventoryError("inventory artifact exceeds hard size maximum")
        value = json.loads(raw.decode("utf-8"))
    except InventoryError:
        raise
    except (OSError, UnicodeError, json.JSONDecodeError) as error:
        raise InventoryError(f"cannot read inventory artifact {path}: {error}") from error
    detected_secret = secret_summary(raw)
    if detected_secret is not None:
        raise InventoryError(
            f"inventory artifact contains high-confidence secret material ({detected_secret})"
        )
    expected_fields = {
        "schema_version", "namespace", "mode", "repo", "manifest", "limits",
        "counts", "deleted_managed_sources", "renamed_managed_sources",
        "duplicate_blob_groups", "files", "exclusions", "exclusions_sha256",
        "inventory_hash",
    }
    if not isinstance(value, dict) or set(value) != expected_fields:
        raise InventoryError("inventory artifact fields do not match schema v2")
    if (
        value.get("schema_version") != SCHEMA_VERSION
        or value.get("namespace") != NAMESPACE
        or value.get("mode") != "committed_tree"
    ):
        raise InventoryError("unsupported inventory artifact contract")
    if value.get("inventory_hash") != inventory_hash(value):
        raise InventoryError("inventory_hash mismatch")
    exclusions = value.get("exclusions")
    files = value.get("files")
    if not isinstance(exclusions, list) or not isinstance(files, list):
        raise InventoryError("inventory files and exclusions must be arrays")
    if len(files) > HARD_MAXIMA["sources"]:
        raise InventoryError("inventory selected-file catalog exceeds hard maximum")
    if len(exclusions) > HARD_MAXIMA["exclusions"]:
        raise InventoryError("inventory exclusion catalog exceeds hard maximum")
    if value.get("exclusions_sha256") != canonical_sha256(exclusions):
        raise InventoryError("exclusions_sha256 mismatch")
    selected_paths = [
        item.get("path")
        for item in files
        if isinstance(item, dict) and isinstance(item.get("path"), str)
    ]
    excluded_paths = [
        item.get("path")
        for item in exclusions
        if isinstance(item, dict) and isinstance(item.get("path"), str)
    ]
    if (
        len(selected_paths) != len(files)
        or len(excluded_paths) != len(exclusions)
        or len(set(selected_paths)) != len(selected_paths)
        or len(set(excluded_paths)) != len(excluded_paths)
        or set(selected_paths) & set(excluded_paths)
    ):
        raise InventoryError("inventory paths are incomplete, duplicated, or overlapping")
    counts = value.get("counts")
    if not isinstance(counts, dict) or (
        counts.get("tree_entries") != len(files) + len(exclusions)
        or counts.get("selected_files") != len(files)
        or counts.get("excluded_files") != len(exclusions)
    ):
        raise InventoryError("inventory counts disagree with its complete partition")
    return value


def pretty_json_bytes(value: object) -> bytes:
    return (
        json.dumps(value, ensure_ascii=True, allow_nan=False, sort_keys=True, indent=2)
        + "\n"
    ).encode("utf-8")


def publish_json_no_clobber(path: Path, value: object, context: str) -> None:
    """Atomically publish a sidecar without ever replacing an existing target."""

    payload = pretty_json_bytes(value)
    path.parent.mkdir(parents=True, exist_ok=True)
    descriptor: int | None = None
    temporary_name: str | None = None
    try:
        descriptor, temporary_name = tempfile.mkstemp(
            prefix=f".{path.name}.", suffix=".tmp", dir=path.parent
        )
        with os.fdopen(descriptor, "wb") as stream:
            descriptor = None
            stream.write(payload)
            stream.flush()
            os.fsync(stream.fileno())
        try:
            # Linking a fully durable inode is an atomic no-replace publication
            # primitive on the same filesystem. Never use rename here: portable
            # rename would silently overwrite an existing reviewed artifact.
            os.link(temporary_name, path)
        except FileExistsError as error:
            try:
                existing = path.read_bytes()
            except OSError as read_error:
                raise InventoryError(
                    f"cannot inspect existing {context}: {read_error}"
                ) from read_error
            if existing != payload:
                raise InventoryError(
                    f"refusing to clobber existing {context}; publish the new "
                    "artifact at a new path"
                ) from error
        os.unlink(temporary_name)
        temporary_name = None
        directory_descriptor = os.open(path.parent, os.O_RDONLY)
        try:
            os.fsync(directory_descriptor)
        finally:
            os.close(directory_descriptor)
    except BaseException:
        if descriptor is not None:
            try:
                os.close(descriptor)
            except OSError:
                pass
        if temporary_name is not None:
            try:
                os.unlink(temporary_name)
            except OSError:
                pass
        # If link succeeded and a later durability operation failed, the final
        # name is complete (never partial). Do not remove it: another reader may
        # already have observed the atomically published artifact.
        try:
            directory_descriptor = os.open(path.parent, os.O_RDONLY)
            try:
                os.fsync(directory_descriptor)
            finally:
                os.close(directory_descriptor)
        except OSError:
            pass
        raise


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--root", default=".")
    parser.add_argument("--manifest", default=".mneme/bootstrap/manifest.json")
    parser.add_argument("--max-files", type=int, default=500)
    parser.add_argument("--max-bytes", type=int, default=DEFAULT_MAX_BYTES)
    parser.add_argument("--output")
    args = parser.parse_args()
    root = Path(args.root).resolve()
    manifest_path, manifest_relative = resolve_sidecar_path(
        root, args.manifest, "--manifest"
    )
    output = construct_inventory(
        root,
        manifest_path,
        manifest_relative,
        max_files=args.max_files,
        max_bytes=args.max_bytes,
    )
    if args.output is not None:
        output_path, _output_relative = resolve_sidecar_path(
            root, args.output, "--output"
        )
        if output_path == manifest_path:
            raise InventoryError("--output must not overwrite the manifest sidecar")
        publish_json_no_clobber(output_path, output, "inventory sidecar")
    else:
        sys.stdout.buffer.write(pretty_json_bytes(output))
    return 0


if __name__ == "__main__":
    try:
        raise SystemExit(main())
    except InventoryError as error:
        print(f"inventory_repo.py: {error}", file=sys.stderr)
        raise SystemExit(2) from error
