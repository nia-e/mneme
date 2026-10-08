#!/usr/bin/env python3
"""Verify a committed review ledger without executing any claimed evidence."""

from __future__ import annotations

import argparse
import hashlib
import json
import os
import re
import stat
import sys
from dataclasses import dataclass
from pathlib import Path, PurePosixPath
from typing import Any, Sequence

if __package__:
    from .review_ledger_git import (
        GitBlobClaim,
        GitCommitClaim,
        GitCustodyError,
        LocatorCheck,
        MAX_TOTAL_LOCATOR_BYTES,
        verify_repository_custody,
    )
else:
    from review_ledger_git import (
        GitBlobClaim,
        GitCommitClaim,
        GitCustodyError,
        LocatorCheck,
        MAX_TOTAL_LOCATOR_BYTES,
        verify_repository_custody,
    )


SCHEMA = "mneme.review-ledger.v1"
DISPOSITIONS = frozenset(
    {
        "confirmed",
        "fixed",
        "already_fixed",
        "invalid",
        "deferred",
        "superseded",
        "not_applied",
    }
)
CLOSURE_TIERS = frozenset(
    {
        "source_proof",
        "component_test",
        "installed_artifact",
        "live_store",
        "evaluation",
    }
)

MAX_LEDGER_BYTES = 8 * 1024 * 1024
# The schema needs only six container levels. Keep decoding comfortably below
# interpreter recursion limits rather than treating those limits as the format.
MAX_JSON_DEPTH = 64
MAX_FINDINGS = 4096
MAX_EVIDENCE_PER_ENTRY = 64
MAX_TOTAL_EVIDENCE = 4096
MAX_DIAGNOSTICS = 50
MAX_PATH_BYTES = 4096
MAX_LITERAL_BYTES = 16 * 1024
MAX_TEXT_BYTES = 16 * 1024

HEX_SHA256 = re.compile(r"[0-9a-f]{64}\Z")
FULL_COMMIT = re.compile(r"(?:[0-9a-f]{40}|[0-9a-f]{64})\Z")
FINDING_ID = re.compile(r"[A-Za-z0-9][A-Za-z0-9._-]{0,127}\Z")


class OperationalError(RuntimeError):
    """An invocation, JSON, Git, or I/O operation could not be completed."""


class DuplicateJsonKey(ValueError):
    """A JSON object contains the same key more than once."""


@dataclass(frozen=True)
class Locator:
    literal: str
    literal_sha256: str


@dataclass(frozen=True)
class Artifact:
    path: str
    artifact_commit: str
    git_blob_sha256: str


@dataclass(frozen=True)
class ResponseArtifact:
    path: str
    response_commit: str
    git_blob_sha256: str


@dataclass(frozen=True)
class SourceFinding:
    finding_id: str
    locator: Locator


@dataclass(frozen=True)
class Evidence:
    role: str
    path: str
    commit: str
    git_blob_sha256: str
    locator: Locator
    description: str


@dataclass(frozen=True)
class FollowUp:
    owner: str
    action: str


@dataclass(frozen=True)
class Entry:
    finding_id: str
    disposition: str
    closure_tier: str
    summary: str
    response_locator: Locator
    evidence: tuple[Evidence, ...]
    rationale: str | None
    follow_up: FollowUp | None


@dataclass(frozen=True)
class Ledger:
    artifact: Artifact
    response: ResponseArtifact
    reviewed_commit: str
    source_findings: tuple[SourceFinding, ...]
    entries: tuple[Entry, ...]


class Diagnostics:
    def __init__(self) -> None:
        self.total = 0
        self.items: list[str] = []

    def add(self, location: str, message: str) -> None:
        self.total += 1
        if len(self.items) < MAX_DIAGNOSTICS:
            self.items.append(f"{location}: {message}")

    def rendered(self) -> list[str]:
        rendered = list(self.items)
        omitted = self.total - len(self.items)
        if omitted:
            rendered.append(f"... {omitted} additional error(s) omitted")
        return rendered


def _duplicate_rejecting_object(pairs: list[tuple[str, Any]]) -> dict[str, Any]:
    result: dict[str, Any] = {}
    for key, value in pairs:
        if key in result:
            raise DuplicateJsonKey(f"duplicate JSON object key {key!r}")
        result[key] = value
    return result


def _reject_json_constant(value: str) -> None:
    raise ValueError(f"non-finite JSON number {value!r} is not permitted")


def _check_json_depth(text: str) -> None:
    """Bound container nesting before decoding; leave grammar to json.loads."""
    depth = 0
    in_string = False
    escaped = False
    for character in text:
        if in_string:
            if escaped:
                escaped = False
            elif character == "\\":
                escaped = True
            elif character == '"':
                in_string = False
        elif character == '"':
            in_string = True
        elif character in "[{":
            depth += 1
            if depth > MAX_JSON_DEPTH:
                raise ValueError(
                    "JSON nesting exceeds the supported depth "
                    f"of {MAX_JSON_DEPTH} container levels"
                )
        elif character in "]}":
            depth -= 1


def load_json(path: Path) -> Any:
    flags = os.O_RDONLY | os.O_NONBLOCK
    if hasattr(os, "O_CLOEXEC"):
        flags |= os.O_CLOEXEC
    try:
        descriptor = os.open(path, flags)
    except OSError as error:
        raise OperationalError(f"cannot open ledger {path}: {error}") from error
    try:
        try:
            before = os.fstat(descriptor)
        except OSError as error:
            raise OperationalError(f"cannot inspect ledger {path}: {error}") from error
        if not stat.S_ISREG(before.st_mode):
            raise OperationalError(f"ledger {path} is not a regular file")
        if before.st_size > MAX_LEDGER_BYTES:
            raise OperationalError(
                f"ledger {path} is {before.st_size} bytes; limit is "
                f"{MAX_LEDGER_BYTES} bytes"
            )

        payload = bytearray()
        while len(payload) <= MAX_LEDGER_BYTES:
            try:
                chunk = os.read(
                    descriptor,
                    min(64 * 1024, MAX_LEDGER_BYTES + 1 - len(payload)),
                )
            except InterruptedError:
                continue
            except OSError as error:
                raise OperationalError(
                    f"cannot read ledger {path}: {error}"
                ) from error
            if not chunk:
                break
            payload.extend(chunk)
        if len(payload) > MAX_LEDGER_BYTES:
            raise OperationalError(
                f"ledger {path} exceeds the {MAX_LEDGER_BYTES}-byte limit"
            )
        try:
            after = os.fstat(descriptor)
        except OSError as error:
            raise OperationalError(
                f"cannot re-inspect ledger {path}: {error}"
            ) from error
        before_identity = (
            before.st_dev,
            before.st_ino,
            before.st_size,
            before.st_mtime_ns,
            before.st_ctime_ns,
        )
        after_identity = (
            after.st_dev,
            after.st_ino,
            after.st_size,
            after.st_mtime_ns,
            after.st_ctime_ns,
        )
        if before_identity != after_identity or len(payload) != after.st_size:
            raise OperationalError(f"ledger {path} changed while it was being read")
    finally:
        os.close(descriptor)
    try:
        text = bytes(payload).decode("utf-8")
    except UnicodeDecodeError as error:
        raise OperationalError(f"ledger {path} is not UTF-8: {error}") from error
    try:
        _check_json_depth(text)
        return json.loads(
            text,
            object_pairs_hook=_duplicate_rejecting_object,
            parse_constant=_reject_json_constant,
        )
    except RecursionError as error:
        raise OperationalError(
            f"cannot parse ledger {path}: JSON nesting exceeds the supported depth"
        ) from error
    except (json.JSONDecodeError, DuplicateJsonKey, ValueError) as error:
        raise OperationalError(
            f"cannot parse ledger {path} as strict JSON: {error}"
        ) from error


def _object(
    value: Any,
    location: str,
    required: frozenset[str],
    optional: frozenset[str],
    diagnostics: Diagnostics,
) -> dict[str, Any] | None:
    if type(value) is not dict:
        diagnostics.add(location, "must be an object")
        return None
    allowed = required | optional
    for key in sorted(value.keys() - allowed):
        diagnostics.add(f"{location}.{key}", "unknown field")
    for key in sorted(required - value.keys()):
        diagnostics.add(f"{location}.{key}", "required field is missing")
    return value


def _array(
    value: Any,
    location: str,
    diagnostics: Diagnostics,
    *,
    maximum: int,
) -> list[Any] | None:
    if type(value) is not list:
        diagnostics.add(location, "must be an array")
        return None
    if not value:
        diagnostics.add(location, "must not be empty")
    if len(value) > maximum:
        diagnostics.add(location, f"has {len(value)} items; limit is {maximum}")
        return value[:maximum]
    return value


def _text(
    value: Any,
    location: str,
    diagnostics: Diagnostics,
    *,
    maximum: int = MAX_TEXT_BYTES,
) -> str | None:
    if type(value) is not str:
        diagnostics.add(location, "must be a string")
        return None
    try:
        encoded = value.encode("utf-8")
    except UnicodeEncodeError:
        diagnostics.add(location, "must contain valid Unicode scalar text")
        return None
    if not value.strip():
        diagnostics.add(location, "must contain non-whitespace text")
        return None
    if len(encoded) > maximum:
        diagnostics.add(location, f"is {len(encoded)} UTF-8 bytes; limit is {maximum}")
        return None
    if "\x00" in value:
        diagnostics.add(location, "must not contain NUL")
        return None
    return value


def _enum(
    value: Any,
    location: str,
    choices: frozenset[str],
    diagnostics: Diagnostics,
) -> str | None:
    text = _text(value, location, diagnostics, maximum=64)
    if text is not None and text not in choices:
        diagnostics.add(location, f"must be one of {', '.join(sorted(choices))}")
        return None
    return text


def _full_commit(value: Any, location: str, diagnostics: Diagnostics) -> str | None:
    text = _text(value, location, diagnostics, maximum=64)
    if text is not None and FULL_COMMIT.fullmatch(text) is None:
        diagnostics.add(location, "must be a lowercase full 40- or 64-hex commit SHA")
        return None
    return text


def _sha256(value: Any, location: str, diagnostics: Diagnostics) -> str | None:
    text = _text(value, location, diagnostics, maximum=64)
    if text is not None and HEX_SHA256.fullmatch(text) is None:
        diagnostics.add(location, "must be exactly 64 lowercase hexadecimal characters")
        return None
    return text


def _repo_path(value: Any, location: str, diagnostics: Diagnostics) -> str | None:
    text = _text(value, location, diagnostics, maximum=MAX_PATH_BYTES)
    if text is None:
        return None
    if text.startswith("/"):
        diagnostics.add(location, "must be repository-relative, not absolute")
        return None
    if "\\" in text:
        diagnostics.add(location, "must use '/' separators, not backslashes")
        return None
    if any(ord(character) < 32 or ord(character) == 127 for character in text):
        diagnostics.add(location, "must not contain control characters")
        return None
    parts = text.split("/")
    if any(part in {"", ".", ".."} for part in parts):
        diagnostics.add(location, "must not contain empty, '.' or '..' components")
        return None
    if PurePosixPath(text).as_posix() != text:
        diagnostics.add(location, "must be a normalized repository-relative path")
        return None
    return text


def _finding_id(value: Any, location: str, diagnostics: Diagnostics) -> str | None:
    text = _text(value, location, diagnostics, maximum=128)
    if text is not None and FINDING_ID.fullmatch(text) is None:
        diagnostics.add(
            location,
            "must start with an ASCII letter or digit and contain only letters, "
            "digits, '.', '_' or '-'",
        )
        return None
    return text


def _parse_locator(
    value: Any, location: str, diagnostics: Diagnostics
) -> Locator | None:
    obj = _object(
        value,
        location,
        frozenset({"literal", "literal_sha256"}),
        frozenset(),
        diagnostics,
    )
    if obj is None:
        return None
    literal = _text(
        obj.get("literal"),
        f"{location}.literal",
        diagnostics,
        maximum=MAX_LITERAL_BYTES,
    )
    digest = _sha256(
        obj.get("literal_sha256"),
        f"{location}.literal_sha256",
        diagnostics,
    )
    if literal is None or digest is None:
        return None
    actual = hashlib.sha256(literal.encode("utf-8")).hexdigest()
    if digest != actual:
        diagnostics.add(
            f"{location}.literal_sha256",
            f"does not match the locator literal (expected {actual})",
        )
    return Locator(literal=literal, literal_sha256=digest)


def _parse_artifact(
    value: Any, location: str, diagnostics: Diagnostics
) -> Artifact | None:
    obj = _object(
        value,
        location,
        frozenset({"path", "artifact_commit", "git_blob_sha256"}),
        frozenset(),
        diagnostics,
    )
    if obj is None:
        return None
    path = _repo_path(obj.get("path"), f"{location}.path", diagnostics)
    commit = _full_commit(
        obj.get("artifact_commit"), f"{location}.artifact_commit", diagnostics
    )
    digest = _sha256(
        obj.get("git_blob_sha256"), f"{location}.git_blob_sha256", diagnostics
    )
    if path is None or commit is None or digest is None:
        return None
    return Artifact(path=path, artifact_commit=commit, git_blob_sha256=digest)


def _parse_response_artifact(
    value: Any, location: str, diagnostics: Diagnostics
) -> ResponseArtifact | None:
    obj = _object(
        value,
        location,
        frozenset({"path", "response_commit", "git_blob_sha256"}),
        frozenset(),
        diagnostics,
    )
    if obj is None:
        return None
    path = _repo_path(obj.get("path"), f"{location}.path", diagnostics)
    commit = _full_commit(
        obj.get("response_commit"), f"{location}.response_commit", diagnostics
    )
    digest = _sha256(
        obj.get("git_blob_sha256"), f"{location}.git_blob_sha256", diagnostics
    )
    if path is None or commit is None or digest is None:
        return None
    return ResponseArtifact(
        path=path, response_commit=commit, git_blob_sha256=digest
    )


def _parse_source_finding(
    value: Any, location: str, diagnostics: Diagnostics
) -> SourceFinding | None:
    obj = _object(
        value,
        location,
        frozenset({"id", "locator"}),
        frozenset(),
        diagnostics,
    )
    if obj is None:
        return None
    finding_id = _finding_id(obj.get("id"), f"{location}.id", diagnostics)
    locator = _parse_locator(obj.get("locator"), f"{location}.locator", diagnostics)
    if finding_id is None or locator is None:
        return None
    return SourceFinding(finding_id=finding_id, locator=locator)


def _parse_evidence(
    value: Any, location: str, diagnostics: Diagnostics
) -> Evidence | None:
    obj = _object(
        value,
        location,
        frozenset(
            {
                "role",
                "path",
                "commit",
                "git_blob_sha256",
                "locator",
                "description",
            }
        ),
        frozenset(),
        diagnostics,
    )
    if obj is None:
        return None
    role = _enum(obj.get("role"), f"{location}.role", CLOSURE_TIERS, diagnostics)
    path = _repo_path(obj.get("path"), f"{location}.path", diagnostics)
    commit = _full_commit(obj.get("commit"), f"{location}.commit", diagnostics)
    digest = _sha256(
        obj.get("git_blob_sha256"), f"{location}.git_blob_sha256", diagnostics
    )
    locator = _parse_locator(obj.get("locator"), f"{location}.locator", diagnostics)
    description = _text(obj.get("description"), f"{location}.description", diagnostics)
    if None in {role, path, commit, digest, locator, description}:
        return None
    assert role is not None
    assert path is not None
    assert commit is not None
    assert digest is not None
    assert locator is not None
    assert description is not None
    return Evidence(
        role=role,
        path=path,
        commit=commit,
        git_blob_sha256=digest,
        locator=locator,
        description=description,
    )


def _parse_follow_up(
    value: Any, location: str, diagnostics: Diagnostics
) -> FollowUp | None:
    obj = _object(
        value,
        location,
        frozenset({"owner", "action"}),
        frozenset(),
        diagnostics,
    )
    if obj is None:
        return None
    owner = _text(obj.get("owner"), f"{location}.owner", diagnostics, maximum=256)
    action = _text(obj.get("action"), f"{location}.action", diagnostics)
    if owner is None or action is None:
        return None
    return FollowUp(owner=owner, action=action)


def _parse_entry(value: Any, location: str, diagnostics: Diagnostics) -> Entry | None:
    obj = _object(
        value,
        location,
        frozenset(
            {
                "finding_id",
                "disposition",
                "closure_tier",
                "summary",
                "response_locator",
                "evidence",
            }
        ),
        frozenset({"rationale", "follow_up"}),
        diagnostics,
    )
    if obj is None:
        return None
    finding_id = _finding_id(
        obj.get("finding_id"), f"{location}.finding_id", diagnostics
    )
    disposition = _enum(
        obj.get("disposition"), f"{location}.disposition", DISPOSITIONS, diagnostics
    )
    closure_tier = _enum(
        obj.get("closure_tier"),
        f"{location}.closure_tier",
        CLOSURE_TIERS,
        diagnostics,
    )
    summary = _text(obj.get("summary"), f"{location}.summary", diagnostics)
    response_locator = _parse_locator(
        obj.get("response_locator"), f"{location}.response_locator", diagnostics
    )
    raw_evidence = _array(
        obj.get("evidence"),
        f"{location}.evidence",
        diagnostics,
        maximum=MAX_EVIDENCE_PER_ENTRY,
    )
    evidence: list[Evidence] = []
    if raw_evidence is not None:
        for index, item in enumerate(raw_evidence):
            parsed = _parse_evidence(item, f"{location}.evidence[{index}]", diagnostics)
            if parsed is not None:
                evidence.append(parsed)

    rationale: str | None = None
    if "rationale" in obj:
        rationale = _text(obj["rationale"], f"{location}.rationale", diagnostics)
    follow_up: FollowUp | None = None
    if "follow_up" in obj:
        follow_up = _parse_follow_up(
            obj["follow_up"], f"{location}.follow_up", diagnostics
        )

    if (
        None in {finding_id, disposition, closure_tier, summary, response_locator}
        or raw_evidence is None
    ):
        return None
    assert finding_id is not None
    assert disposition is not None
    assert closure_tier is not None
    assert summary is not None
    assert response_locator is not None
    entry = Entry(
        finding_id=finding_id,
        disposition=disposition,
        closure_tier=closure_tier,
        summary=summary,
        response_locator=response_locator,
        evidence=tuple(evidence),
        rationale=rationale,
        follow_up=follow_up,
    )
    _validate_entry_rules(entry, location, diagnostics)
    return entry


def _validate_entry_rules(
    entry: Entry, location: str, diagnostics: Diagnostics
) -> None:
    roles = {evidence.role for evidence in entry.evidence}
    if "source_proof" not in roles:
        diagnostics.add(f"{location}.evidence", "must include a source_proof record")
    if entry.closure_tier not in roles:
        diagnostics.add(
            f"{location}.evidence",
            f"must include a record whose role matches closure_tier "
            f"{entry.closure_tier!r}",
        )

    if entry.disposition in {"fixed", "already_fixed"}:
        if entry.closure_tier == "source_proof":
            diagnostics.add(
                f"{location}.closure_tier",
                f"{entry.disposition} requires component_test or a distinct "
                "runtime/evaluation tier",
            )
        if "component_test" not in roles:
            diagnostics.add(
                f"{location}.evidence",
                f"{entry.disposition} must include a component_test record",
            )

    if (
        entry.disposition in {"confirmed", "deferred"}
        and entry.closure_tier != "source_proof"
    ):
        diagnostics.add(
            f"{location}.closure_tier",
            f"open disposition {entry.disposition!r} must declare source_proof",
        )

    if entry.disposition == "superseded" and entry.closure_tier != "source_proof":
        if "component_test" not in roles:
            diagnostics.add(
                f"{location}.evidence",
                "superseded with a non-source closure tier must include a "
                "component_test record",
            )

    requires_rationale = entry.disposition in {
        "invalid",
        "deferred",
        "superseded",
        "not_applied",
    }
    if requires_rationale and entry.rationale is None:
        diagnostics.add(
            f"{location}.rationale",
            f"disposition {entry.disposition!r} requires a rationale",
        )
    if not requires_rationale and entry.rationale is not None:
        diagnostics.add(
            f"{location}.rationale",
            f"is not permitted for disposition {entry.disposition!r}",
        )

    requires_follow_up = entry.disposition in {"confirmed", "deferred"}
    if requires_follow_up and entry.follow_up is None:
        diagnostics.add(
            f"{location}.follow_up",
            f"open disposition {entry.disposition!r} requires owner and action",
        )
    if not requires_follow_up and entry.follow_up is not None:
        diagnostics.add(
            f"{location}.follow_up",
            f"is not permitted for disposition {entry.disposition!r}",
        )

    seen_evidence: set[tuple[str, str, str, str]] = set()
    for index, evidence in enumerate(entry.evidence):
        identity = (
            evidence.role,
            evidence.commit,
            evidence.path,
            evidence.locator.literal,
        )
        if identity in seen_evidence:
            diagnostics.add(
                f"{location}.evidence[{index}]",
                "duplicates an earlier evidence record",
            )
        seen_evidence.add(identity)


def parse_ledger(value: Any, diagnostics: Diagnostics) -> Ledger | None:
    obj = _object(
        value,
        "$",
        frozenset(
            {
                "schema",
                "artifact",
                "response",
                "reviewed_commit",
                "source_findings",
                "entries",
            }
        ),
        frozenset(),
        diagnostics,
    )
    if obj is None:
        return None
    schema = _text(obj.get("schema"), "$.schema", diagnostics, maximum=64)
    if schema is not None and schema != SCHEMA:
        diagnostics.add("$.schema", f"must be exactly {SCHEMA!r}")
    artifact = _parse_artifact(obj.get("artifact"), "$.artifact", diagnostics)
    response = _parse_response_artifact(
        obj.get("response"), "$.response", diagnostics
    )
    reviewed_commit = _full_commit(
        obj.get("reviewed_commit"), "$.reviewed_commit", diagnostics
    )

    raw_findings = _array(
        obj.get("source_findings"),
        "$.source_findings",
        diagnostics,
        maximum=MAX_FINDINGS,
    )
    source_findings: list[SourceFinding] = []
    if raw_findings is not None:
        for index, item in enumerate(raw_findings):
            parsed = _parse_source_finding(
                item, f"$.source_findings[{index}]", diagnostics
            )
            if parsed is not None:
                source_findings.append(parsed)

    raw_entries = _array(
        obj.get("entries"), "$.entries", diagnostics, maximum=MAX_FINDINGS
    )
    entries: list[Entry] = []
    evidence_limit_exceeded = False
    if raw_entries is not None:
        raw_evidence_count = 0
        for item in raw_entries:
            if type(item) is not dict:
                continue
            raw_evidence = item.get("evidence")
            if type(raw_evidence) is not list:
                continue
            raw_evidence_count += len(raw_evidence)
            if raw_evidence_count > MAX_TOTAL_EVIDENCE:
                evidence_limit_exceeded = True
                diagnostics.add(
                    "$.entries",
                    f"contain more than {MAX_TOTAL_EVIDENCE} evidence records",
                )
                break

    if raw_entries is not None and not evidence_limit_exceeded:
        for index, item in enumerate(raw_entries):
            parsed = _parse_entry(item, f"$.entries[{index}]", diagnostics)
            if parsed is not None:
                entries.append(parsed)

    source_ids: set[str] = set()
    source_locators: set[str] = set()
    for index, finding in enumerate(source_findings):
        if finding.finding_id in source_ids:
            diagnostics.add(
                f"$.source_findings[{index}].id",
                "duplicates an earlier source finding ID",
            )
        source_ids.add(finding.finding_id)
        if finding.locator.literal in source_locators:
            diagnostics.add(
                f"$.source_findings[{index}].locator.literal",
                "duplicates an earlier source finding locator",
            )
        source_locators.add(finding.locator.literal)

    entry_ids: set[str] = set()
    response_locators: set[str] = set()
    for index, entry in enumerate(entries):
        if entry.finding_id in entry_ids:
            diagnostics.add(
                f"$.entries[{index}].finding_id", "duplicates an earlier ledger entry"
            )
        entry_ids.add(entry.finding_id)
        if entry.response_locator.literal in response_locators:
            diagnostics.add(
                f"$.entries[{index}].response_locator.literal",
                "duplicates an earlier response locator",
            )
        response_locators.add(entry.response_locator.literal)

    for finding_id in sorted(source_ids - entry_ids):
        diagnostics.add("$.entries", f"missing entry for source finding {finding_id!r}")
    for finding_id in sorted(entry_ids - source_ids):
        diagnostics.add(
            "$.entries", f"entry {finding_id!r} has no matching source finding"
        )

    locator_bytes = sum(
        len(finding.locator.literal.encode("utf-8")) for finding in source_findings
    )
    locator_bytes += sum(
        len(entry.response_locator.literal.encode("utf-8"))
        + sum(
            len(evidence.locator.literal.encode("utf-8"))
            for evidence in entry.evidence
        )
        for entry in entries
    )
    if locator_bytes > MAX_TOTAL_LOCATOR_BYTES:
        diagnostics.add(
            "$",
            f"contains {locator_bytes} locator UTF-8 bytes; limit is "
            f"{MAX_TOTAL_LOCATOR_BYTES}",
        )

    if (
        schema is None
        or artifact is None
        or response is None
        or reviewed_commit is None
        or raw_findings is None
        or raw_entries is None
        or evidence_limit_exceeded
    ):
        return None
    return Ledger(
        artifact=artifact,
        response=response,
        reviewed_commit=reviewed_commit,
        source_findings=tuple(source_findings),
        entries=tuple(entries),
    )



def verify_git_evidence(
    repository_path: Path, ledger: Ledger, diagnostics: Diagnostics
) -> None:
    commit_claims = [GitCommitClaim(ledger.reviewed_commit, "$.reviewed_commit")]
    blob_claims = [
        GitBlobClaim(
            commit=ledger.artifact.artifact_commit,
            commit_location="$.artifact.artifact_commit",
            path=ledger.artifact.path,
            location="$.artifact",
            expected_digest=ledger.artifact.git_blob_sha256,
            locators=tuple(
                LocatorCheck(
                    literal=finding.locator.literal,
                    location=f"$.source_findings[{index}].locator.literal",
                )
                for index, finding in enumerate(ledger.source_findings)
            ),
        ),
        GitBlobClaim(
            commit=ledger.response.response_commit,
            commit_location="$.response.response_commit",
            path=ledger.response.path,
            location="$.response",
            expected_digest=ledger.response.git_blob_sha256,
            locators=tuple(
                LocatorCheck(
                    literal=entry.response_locator.literal,
                    location=f"$.entries[{index}].response_locator.literal",
                )
                for index, entry in enumerate(ledger.entries)
            ),
        ),
    ]
    for entry_index, entry in enumerate(ledger.entries):
        for evidence_index, evidence in enumerate(entry.evidence):
            location = f"$.entries[{entry_index}].evidence[{evidence_index}]"
            blob_claims.append(
                GitBlobClaim(
                    commit=evidence.commit,
                    commit_location=f"{location}.commit",
                    path=evidence.path,
                    location=location,
                    expected_digest=evidence.git_blob_sha256,
                    locators=(
                        LocatorCheck(
                            literal=evidence.locator.literal,
                            location=f"{location}.locator.literal",
                        ),
                    ),
                )
            )

    verify_repository_custody(
        repository_path, commit_claims, blob_claims, diagnostics
    )


def _result_payload(
    *,
    valid: bool,
    kind: str,
    ledger: Ledger | None,
    diagnostics: Diagnostics,
) -> dict[str, Any]:
    return {
        "valid": valid,
        "kind": kind,
        "ledger_schema": SCHEMA if ledger is not None else None,
        "finding_count": len(ledger.source_findings) if ledger is not None else 0,
        "entry_count": len(ledger.entries) if ledger is not None else 0,
        "evidence_count": (
            sum(len(entry.evidence) for entry in ledger.entries)
            if ledger is not None
            else 0
        ),
        "error_count": diagnostics.total,
        "errors": diagnostics.rendered(),
    }


def parse_args(argv: Sequence[str] | None = None) -> argparse.Namespace:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--repo", required=True, type=Path, help="local Git repository")
    parser.add_argument("--ledger", required=True, type=Path, help="review ledger JSON")
    parser.add_argument(
        "--json", action="store_true", help="emit one JSON result object"
    )
    return parser.parse_args(argv)


def main(argv: Sequence[str] | None = None) -> int:
    args = parse_args(argv)
    diagnostics = Diagnostics()
    ledger: Ledger | None = None
    try:
        raw = load_json(args.ledger)
        ledger = parse_ledger(raw, diagnostics)
        if ledger is not None and diagnostics.total == 0:
            verify_git_evidence(args.repo, ledger, diagnostics)
    except (OperationalError, GitCustodyError, RecursionError) as error:
        message = (
            "ledger nesting exceeds the supported recursion depth"
            if isinstance(error, RecursionError)
            else str(error)
        )
        diagnostics.add("operation", message)
        if args.json:
            print(
                json.dumps(
                    _result_payload(
                        valid=False,
                        kind="operational",
                        ledger=ledger,
                        diagnostics=diagnostics,
                    ),
                    sort_keys=True,
                    separators=(",", ":"),
                )
            )
        else:
            for diagnostic in diagnostics.rendered():
                print(f"error: {diagnostic}", file=sys.stderr)
        return 2

    valid = diagnostics.total == 0
    payload = _result_payload(
        valid=valid,
        kind="verification",
        ledger=ledger,
        diagnostics=diagnostics,
    )
    if args.json:
        print(json.dumps(payload, sort_keys=True, separators=(",", ":")))
    elif valid:
        assert ledger is not None
        evidence_count = sum(len(entry.evidence) for entry in ledger.entries)
        print(
            f"valid {SCHEMA}: {len(ledger.source_findings)} finding(s), "
            f"{evidence_count} evidence record(s)"
        )
    else:
        for diagnostic in diagnostics.rendered():
            print(f"error: {diagnostic}", file=sys.stderr)
    return 0 if valid else 1


if __name__ == "__main__":
    raise SystemExit(main())
