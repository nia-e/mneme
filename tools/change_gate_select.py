#!/usr/bin/env python3
"""Select Mneme change-gate packs from explicit semantic declarations."""

from __future__ import annotations

import argparse
import json
import sys
from dataclasses import dataclass
from pathlib import PurePosixPath
from typing import Any, Sequence


SCHEMA = "mneme.change-gates.v1"
SKILL_ROOT = ".agents/skills/mneme-change-gates"
MAX_PATH_BYTES = 4096
MAX_TOUCHED_PATHS = 4096
MAX_ATTESTATION_BYTES = 2048

EVIDENCE_ORDER = {
    "source_proof": 0,
    "component_test": 1,
    "installed_artifact": 2,
    "live_store": 3,
    "evaluation": 4,
}


@dataclass(frozen=True)
class GatePack:
    operation_class: str
    reference: str
    required_gates: tuple[str, ...]
    required_evidence_floor: str
    downstream_routing: tuple[str, ...]


PACKS = (
    GatePack(
        "public-interface",
        f"{SKILL_ROOT}/references/public-interface.md",
        ("API-01", "API-02", "API-03", "API-04", "API-05"),
        "component_test",
        (),
    ),
    GatePack(
        "capability",
        f"{SKILL_ROOT}/references/capability.md",
        ("CAP-01", "CAP-02", "CAP-03", "CAP-04", "CAP-05"),
        "component_test",
        (),
    ),
    GatePack(
        "transport-package",
        f"{SKILL_ROOT}/references/transport-package.md",
        ("DIST-01", "DIST-02", "DIST-03", "DIST-04", "DIST-05"),
        "installed_artifact",
        (),
    ),
    GatePack(
        "storage-admission",
        f"{SKILL_ROOT}/references/storage-admission.md",
        ("OPEN-01", "OPEN-02", "OPEN-03", "OPEN-04", "OPEN-05", "OPEN-06"),
        "component_test",
        (),
    ),
    GatePack(
        "storage-publication",
        f"{SKILL_ROOT}/references/storage-publication.md",
        (
            "PUBLISH-01",
            "PUBLISH-02",
            "PUBLISH-03",
            "PUBLISH-04",
            "PUBLISH-05",
            "PUBLISH-06",
            "PUBLISH-07",
        ),
        "component_test",
        (),
    ),
    GatePack(
        "stale-writer-compatibility",
        f"{SKILL_ROOT}/references/stale-writer-compatibility.md",
        (
            "COMPAT-01",
            "COMPAT-02",
            "COMPAT-03",
            "COMPAT-04",
            "COMPAT-05",
            "COMPAT-06",
        ),
        "component_test",
        (),
    ),
)

PACK_BY_CLASS = {pack.operation_class: pack for pack in PACKS}
CLASS_ORDER = {pack.operation_class: index for index, pack in enumerate(PACKS)}


@dataclass(frozen=True)
class PathRule:
    match: str
    pattern: str
    declaration: str
    classes: tuple[str, ...]
    reason: str

    def applies(self, path: str) -> bool:
        if self.match == "exact":
            return path == self.pattern
        if self.match == "prefix":
            return path.startswith(self.pattern)
        if self.match == "suffix":
            return path.endswith(self.pattern)
        raise AssertionError(f"unknown path-rule match kind {self.match!r}")


# These rules are deliberately sparse. They identify only path regions whose
# trust boundary is stable enough to validate a declaration. They never add an
# operation class: an omitted or ambiguous declaration is an error instead.
PATH_RULES = (
    PathRule(
        "exact",
        "crates/mneme-mcp/src/response.rs",
        "all",
        ("public-interface",),
        "MCP response shapes are a public interface",
    ),
    PathRule(
        "exact",
        "crates/mneme-mcp/src/http.rs",
        "all",
        ("transport-package",),
        "the Streamable HTTP implementation is a transport boundary",
    ),
    PathRule(
        "exact",
        "crates/mnemed/src/repl.rs",
        "all",
        ("public-interface",),
        "REPL commands and errors are a public CLI interface",
    ),
    PathRule(
        "exact",
        "crates/mneme-store-path/src/fresh.rs",
        "all",
        ("storage-publication",),
        "fresh-store code owns publication and recovery authority",
    ),
    PathRule(
        "prefix",
        "crates/mneme-store-path/src/fresh/",
        "all",
        ("storage-publication",),
        "fresh-store code owns publication and recovery authority",
    ),
    PathRule(
        "exact",
        "crates/mneme-cozo/src/cozo_store/fresh_current.rs",
        "all",
        ("storage-publication",),
        "fresh-current materialization crosses the publication boundary",
    ),
    PathRule(
        "exact",
        "crates/mneme-cozo/src/cozo_store/opening.rs",
        "all",
        ("storage-admission",),
        "persistent open classifies and admits stores",
    ),
    PathRule(
        "prefix",
        "crates/mneme-cozo/src/cozo_store/opening/",
        "all",
        ("storage-admission",),
        "persistent open classifies and admits stores",
    ),
    PathRule(
        "prefix",
        "crates/mneme-cozo/src/storage_contract/conventional_unmanaged/",
        "all",
        ("storage-admission",),
        "the conventional-unmanaged contract governs store admission",
    ),
    PathRule(
        "exact",
        "crates/mneme-store-path/src/lib.rs",
        "any",
        ("storage-admission", "storage-publication"),
        "the shared path module contains both admission and publication seams",
    ),
    PathRule(
        "exact",
        "crates/mneme-cozo/src/cozo_store/maintenance.rs",
        "any",
        ("storage-publication", "stale-writer-compatibility"),
        "persistent maintenance may publish a generation or fence old writers",
    ),
    PathRule(
        "exact",
        "crates/mneme-cozo/src/tag_projection.rs",
        "any",
        ("storage-publication", "stale-writer-compatibility"),
        "tag projection changes may publish schema or alter compatibility",
    ),
    PathRule(
        "exact",
        "crates/mneme-cozo/src/vector_projection.rs",
        "any",
        ("storage-publication", "stale-writer-compatibility"),
        "vector projection changes may publish schema or alter compatibility",
    ),
    PathRule(
        "exact",
        "crates/mnemed/src/bootstrap.rs",
        "any",
        (
            "public-interface",
            "storage-admission",
            "storage-publication",
            "stale-writer-compatibility",
        ),
        "native bootstrap multiplexes CLI, lease, publication, and retry boundaries",
    ),
    PathRule(
        "exact",
        "crates/mneme-mcp/src/main.rs",
        "any",
        (
            "public-interface",
            "capability",
            "transport-package",
            "storage-admission",
            "storage-publication",
            "stale-writer-compatibility",
        ),
        "the MCP entry point multiplexes several semantic boundaries",
    ),
    PathRule(
        "exact",
        "crates/mneme-mcp/src/host.rs",
        "any",
        (
            "public-interface",
            "capability",
            "storage-admission",
            "storage-publication",
            "stale-writer-compatibility",
        ),
        "the MCP host multiplexes public, capability, and storage boundaries",
    ),
    PathRule(
        "exact",
        "crates/mnemed/src/main.rs",
        "any",
        (
            "public-interface",
            "storage-admission",
            "storage-publication",
            "stale-writer-compatibility",
        ),
        "the CLI entry point multiplexes public and storage boundaries",
    ),
    PathRule(
        "exact",
        "tools/mcp_stdio_smoke.py",
        "any",
        ("public-interface", "capability", "transport-package"),
        "the installed stdio smoke covers public, capability, and transport contracts",
    ),
    PathRule(
        "prefix",
        ".github/workflows/",
        "all",
        ("transport-package",),
        "release and CI workflows affect the shipped artifact matrix",
    ),
    PathRule(
        "exact",
        "Cargo.lock",
        "all",
        ("transport-package",),
        "the lockfile defines shipped dependency resolution",
    ),
    PathRule(
        "exact",
        "Cargo.toml",
        "all",
        ("transport-package",),
        "the workspace manifest defines features and packaged artifacts",
    ),
    PathRule(
        "suffix",
        "/Cargo.toml",
        "all",
        ("transport-package",),
        "crate manifests define features and packaged artifacts",
    ),
)


def class_sort_key(operation_class: str) -> int:
    return CLASS_ORDER[operation_class]


def normalize_path(raw_path: str) -> tuple[str | None, str | None]:
    if not raw_path:
        return None, "touched path is empty"
    if len(raw_path.encode("utf-8")) > MAX_PATH_BYTES:
        return None, f"touched path exceeds {MAX_PATH_BYTES} UTF-8 bytes"
    if any(ord(character) < 32 or ord(character) == 127 for character in raw_path):
        return None, "touched path contains a control character"
    if "\\" in raw_path:
        return None, "touched path must use repository-relative '/' separators"

    path = PurePosixPath(raw_path)
    if path.is_absolute():
        return None, "touched path must be repository-relative"
    if ".." in path.parts:
        return None, "touched path may not contain '..'"
    normalized = path.as_posix()
    if normalized in ("", "."):
        return None, "touched path must name a repository entry"
    return normalized, None


def diagnostic(
    code: str,
    message: str,
    *,
    path: str | None = None,
    classes: Sequence[str] = (),
) -> dict[str, Any]:
    result: dict[str, Any] = {"code": code, "message": message}
    if path is not None:
        result["path"] = path
    if classes:
        result["classes"] = list(classes)
    return result


def pack_record(pack: GatePack) -> dict[str, Any]:
    return {
        "operation_class": pack.operation_class,
        "reference": pack.reference,
        "required_gates": list(pack.required_gates),
        "required_evidence_floor": pack.required_evidence_floor,
        "downstream_routing": list(pack.downstream_routing),
    }


def select_change_gates(
    declared_classes: Sequence[str],
    touched_paths: Sequence[str],
    path_attestations: Sequence[tuple[str, str]] = (),
) -> dict[str, Any]:
    errors: list[dict[str, Any]] = []
    unknown_classes = sorted(set(declared_classes) - PACK_BY_CLASS.keys())
    for operation_class in unknown_classes:
        errors.append(
            diagnostic(
                "unknown_class",
                f"unknown operation class {operation_class!r}",
                classes=(operation_class,),
            )
        )

    selected_class_set = set(declared_classes) & PACK_BY_CLASS.keys()
    selected_classes = sorted(selected_class_set, key=class_sort_key)
    if not declared_classes:
        errors.append(
            diagnostic(
                "missing_class",
                "declare at least one operation class; paths cannot grant semantic authority",
            )
        )

    normalized_paths: set[str] = set()
    if not touched_paths:
        errors.append(
            diagnostic(
                "missing_path",
                "provide at least one touched repository path",
            )
        )
    if len(touched_paths) > MAX_TOUCHED_PATHS:
        errors.append(
            diagnostic(
                "too_many_paths",
                f"at most {MAX_TOUCHED_PATHS} touched paths are accepted",
            )
        )
    else:
        for raw_path in touched_paths:
            normalized, path_error = normalize_path(raw_path)
            if path_error is not None:
                errors.append(
                    diagnostic(
                        "invalid_path",
                        f"invalid touched path {raw_path!r}: {path_error}",
                        path=raw_path,
                    )
                )
            else:
                assert normalized is not None
                normalized_paths.add(normalized)

    normalized_attestations: dict[str, str] = {}
    for raw_path, raw_rationale in path_attestations:
        normalized, path_error = normalize_path(raw_path)
        if path_error is not None:
            errors.append(
                diagnostic(
                    "invalid_attestation_path",
                    f"invalid attestation path {raw_path!r}: {path_error}",
                    path=raw_path,
                )
            )
            continue
        assert normalized is not None
        rationale = raw_rationale.strip()
        if not rationale:
            errors.append(
                diagnostic(
                    "invalid_attestation",
                    f"attestation for {normalized} requires a nonempty rationale",
                    path=normalized,
                )
            )
            continue
        if len(rationale.encode("utf-8")) > MAX_ATTESTATION_BYTES:
            errors.append(
                diagnostic(
                    "invalid_attestation",
                    f"attestation for {normalized} exceeds {MAX_ATTESTATION_BYTES} UTF-8 bytes",
                    path=normalized,
                )
            )
            continue
        if any(ord(character) < 32 or ord(character) == 127 for character in rationale):
            errors.append(
                diagnostic(
                    "invalid_attestation",
                    f"attestation for {normalized} contains a control character",
                    path=normalized,
                )
            )
            continue
        previous = normalized_attestations.get(normalized)
        if previous is not None and previous != rationale:
            errors.append(
                diagnostic(
                    "conflicting_attestation",
                    f"multiple rationales were provided for {normalized}",
                    path=normalized,
                )
            )
            continue
        normalized_attestations[normalized] = rationale

    for path in sorted(normalized_attestations.keys() - normalized_paths):
        errors.append(
            diagnostic(
                "attestation_path_not_touched",
                f"attestation path {path} is not in the touched-path set",
                path=path,
            )
        )

    path_validation: list[dict[str, Any]] = []
    unclassified_paths: list[str] = []
    attestation_required_paths: set[str] = set()
    omissions: set[str] = set()
    for path in sorted(normalized_paths):
        rules = [rule for rule in PATH_RULES if rule.applies(path)]
        if not rules:
            unclassified_paths.append(path)
            attestation_required_paths.add(path)
            path_validation.append(
                {
                    "path": path,
                    "requirements": [],
                    "attestation_required": True,
                    "attestation": normalized_attestations.get(path),
                }
            )
            continue

        requirements: list[dict[str, Any]] = []
        for rule in rules:
            ordered_classes = tuple(sorted(rule.classes, key=class_sort_key))
            requirements.append(
                {
                    "declaration": rule.declaration,
                    "classes": list(ordered_classes),
                    "reason": rule.reason,
                }
            )
            if rule.declaration == "all":
                missing = tuple(
                    operation_class
                    for operation_class in ordered_classes
                    if operation_class not in selected_class_set
                )
                if missing:
                    omissions.update(missing)
                    errors.append(
                        diagnostic(
                            "declaration_conflict",
                            f"{path} requires explicit declaration of: {', '.join(missing)}",
                            path=path,
                            classes=missing,
                        )
                    )
            elif not selected_class_set.intersection(ordered_classes):
                omission = "one-of:" + ",".join(ordered_classes)
                omissions.add(omission)
                errors.append(
                    diagnostic(
                        "ambiguous_path_authority",
                        f"{path} spans multiple boundaries; explicitly declare one or more of: "
                        + ", ".join(ordered_classes),
                        path=path,
                        classes=ordered_classes,
                    )
                )
            if rule.declaration == "any":
                attestation_required_paths.add(path)
        path_validation.append(
            {
                "path": path,
                "requirements": requirements,
                "attestation_required": path in attestation_required_paths,
                "attestation": normalized_attestations.get(path),
            }
        )

    for path in sorted(normalized_attestations.keys() - attestation_required_paths):
        if path in normalized_paths:
            errors.append(
                diagnostic(
                    "unneeded_attestation",
                    f"{path} has an exact class requirement and needs no semantic attestation",
                    path=path,
                )
            )

    pending_attestations = [
        {
            "path": path,
            "reason": (
                "path has no stable class rule"
                if path in unclassified_paths
                else "path multiplexes multiple semantic boundaries"
            ),
        }
        for path in sorted(attestation_required_paths - normalized_attestations.keys())
    ]

    selected_packs = [pack_record(PACK_BY_CLASS[name]) for name in selected_classes]
    required_gates = [
        {"id": gate, "pack": pack.operation_class}
        for pack in (PACK_BY_CLASS[name] for name in selected_classes)
        for gate in pack.required_gates
    ]
    downstream_routing = sorted(
        {
            route
            for operation_class in selected_classes
            for route in PACK_BY_CLASS[operation_class].downstream_routing
        }
    )
    required_evidence_floor = None
    if selected_classes:
        required_evidence_floor = max(
            (
                PACK_BY_CLASS[operation_class].required_evidence_floor
                for operation_class in selected_classes
            ),
            key=EVIDENCE_ORDER.__getitem__,
        )

    status = "error"
    if not errors:
        status = "needs-attestation" if pending_attestations else "selected"

    return {
        "schema": SCHEMA,
        "status": status,
        "declared_classes": selected_classes,
        "touched_paths": sorted(normalized_paths),
        "selected_packs": selected_packs,
        "required_gates": required_gates,
        "required_evidence_floor": required_evidence_floor,
        "downstream_routing": downstream_routing,
        "omissions": sorted(omissions),
        "unclassified_paths": unclassified_paths,
        "path_attestations": [
            {"path": path, "rationale": rationale}
            for path, rationale in sorted(normalized_attestations.items())
        ],
        "pending_attestations": pending_attestations,
        "path_validation": path_validation,
        "errors": errors,
    }


def render_human(result: dict[str, Any]) -> str:
    lines = [f"status: {result['status']}"]
    if result["errors"]:
        lines.append("errors:")
        for error in result["errors"]:
            lines.append(f"  - [{error['code']}] {error['message']}")

    lines.append("declared classes:")
    lines.extend(f"  - {item}" for item in result["declared_classes"])
    if not result["declared_classes"]:
        lines.append("  - none")

    lines.append("touched paths:")
    lines.extend(f"  - {item}" for item in result["touched_paths"])
    if not result["touched_paths"]:
        lines.append("  - none")

    lines.append("selected packs:")
    for pack in result["selected_packs"]:
        gates = ", ".join(pack["required_gates"])
        lines.append(
            f"  - {pack['operation_class']}: {pack['reference']} "
            f"(gates: {gates}; evidence floor: {pack['required_evidence_floor']})"
        )
    if not result["selected_packs"]:
        lines.append("  - none")

    lines.append(
        f"required evidence floor: {result['required_evidence_floor'] or 'none'}"
    )
    lines.append("downstream routing:")
    lines.extend(f"  - {item}" for item in result["downstream_routing"])
    if not result["downstream_routing"]:
        lines.append("  - none")

    lines.append("omissions:")
    lines.extend(f"  - {item}" for item in result["omissions"])
    if not result["omissions"]:
        lines.append("  - none")

    lines.append("unclassified paths:")
    lines.extend(f"  - {item}" for item in result["unclassified_paths"])
    if not result["unclassified_paths"]:
        lines.append("  - none")
    lines.append("path attestations:")
    lines.extend(
        f"  - {item['path']}: {item['rationale']}"
        for item in result["path_attestations"]
    )
    if not result["path_attestations"]:
        lines.append("  - none")
    lines.append("pending attestations:")
    lines.extend(
        f"  - {item['path']}: {item['reason']}"
        for item in result["pending_attestations"]
    )
    if not result["pending_attestations"]:
        lines.append("  - none")
    return "\n".join(lines) + "\n"


def build_parser() -> argparse.ArgumentParser:
    parser = argparse.ArgumentParser(
        description=(
            "Select Mneme safety-gate packs from explicit operation classes and "
            "validate them against touched paths."
        )
    )
    parser.add_argument(
        "--class",
        dest="operation_classes",
        action="append",
        default=[],
        metavar="CLASS",
        help="declared semantic operation class; repeat for composite changes",
    )
    parser.add_argument(
        "--path",
        dest="touched_paths",
        action="append",
        default=[],
        metavar="REPO_PATH",
        help="touched repository-relative path; repeat for every path",
    )
    parser.add_argument(
        "--attest-path",
        dest="path_attestations",
        action="append",
        nargs=2,
        default=[],
        metavar=("REPO_PATH", "RATIONALE"),
        help=(
            "record why the declared classes cover one unclassified or multiplexed "
            "touched path; repeat as needed"
        ),
    )
    parser.add_argument(
        "--json",
        action="store_true",
        help="emit deterministic machine-readable JSON",
    )
    return parser


def main(argv: Sequence[str] | None = None) -> int:
    args = build_parser().parse_args(argv)
    result = select_change_gates(
        args.operation_classes, args.touched_paths, args.path_attestations
    )
    if args.json:
        sys.stdout.write(json.dumps(result, indent=2, sort_keys=True) + "\n")
    else:
        stream = sys.stdout if result["status"] == "selected" else sys.stderr
        stream.write(render_human(result))
    return 0 if result["status"] == "selected" else 2


if __name__ == "__main__":
    raise SystemExit(main())
