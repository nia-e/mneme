"""Bounded, offline Git custody verification for review ledgers."""

from __future__ import annotations

import hashlib
import os
import re
import select
import subprocess
import time
from dataclasses import dataclass
from pathlib import Path
from typing import Callable, Protocol, Sequence


MAX_BLOB_BYTES = 64 * 1024 * 1024
MAX_TOTAL_LOCATOR_BYTES = 4 * 1024 * 1024
MAX_TOTAL_UNIQUE_BLOB_BYTES = 256 * 1024 * 1024
MAX_TOTAL_LOCATOR_SCAN_BYTES = 512 * 1024 * 1024
MAX_GIT_BATCH_INPUT_BYTES = 16 * 1024 * 1024
MAX_GIT_COMMANDS = 8
GIT_TIMEOUT_SECONDS = 30
GIT_TOTAL_TIMEOUT_SECONDS = 90


class GitCustodyError(RuntimeError):
    """Local Git custody verification could not be completed safely."""


class DiagnosticSink(Protocol):
    def add(self, location: str, message: str) -> None:
        """Record a failed evidence claim."""


@dataclass(frozen=True)
class GitCommitClaim:
    commit: str
    location: str


@dataclass(frozen=True)
class LocatorCheck:
    literal: str
    location: str


@dataclass(frozen=True)
class GitBlobClaim:
    commit: str
    commit_location: str
    path: str
    location: str
    expected_digest: str
    locators: tuple[LocatorCheck, ...]


@dataclass(frozen=True)
class ReferenceUse:
    location: str
    expected_digest: str
    locators: tuple[LocatorCheck, ...]


@dataclass(frozen=True)
class GitObjectInfo:
    object_id: str
    object_type: str
    size: int


@dataclass
class BlobGroup:
    object_id: str
    size: int
    uses: list[ReferenceUse]


class GitExecutionBudget:
    """A single verifier-wide command, control-input, and wall-clock budget."""

    def __init__(
        self,
        *,
        max_commands: int = MAX_GIT_COMMANDS,
        total_seconds: float = GIT_TOTAL_TIMEOUT_SECONDS,
        clock: Callable[[], float] = time.monotonic,
    ) -> None:
        self.max_commands = max_commands
        self.total_seconds = total_seconds
        self.clock = clock
        self.deadline = clock() + total_seconds
        self.command_count = 0
        self.batch_input_bytes = 0

    def claim_command(self) -> float:
        if self.command_count >= self.max_commands:
            raise GitCustodyError(
                f"Git command budget of {self.max_commands} was exhausted"
            )
        remaining = self.deadline - self.clock()
        if remaining <= 0:
            raise GitCustodyError(
                f"Git verification exceeded its {self.total_seconds:g}-second "
                "global deadline"
            )
        self.command_count += 1
        return min(float(GIT_TIMEOUT_SECONDS), remaining)

    def charge_batch_input(self, byte_count: int) -> None:
        self.batch_input_bytes += byte_count
        if self.batch_input_bytes > MAX_GIT_BATCH_INPUT_BYTES:
            raise GitCustodyError(
                "Git batch control input exceeds the cumulative "
                f"{MAX_GIT_BATCH_INPUT_BYTES}-byte limit"
            )


class _DeadlinePipeReader:
    def __init__(self, descriptor: int, deadline: float) -> None:
        self.descriptor = descriptor
        self.deadline = deadline
        self.buffer = bytearray()
        os.set_blocking(descriptor, False)

    def _fill(self) -> None:
        remaining = self.deadline - time.monotonic()
        if remaining <= 0:
            raise GitCustodyError("Git batch command exceeded its deadline")
        try:
            readable, _, _ = select.select([self.descriptor], [], [], remaining)
        except InterruptedError:
            return
        except OSError as error:
            raise GitCustodyError(
                f"cannot poll Git batch output: {error}"
            ) from error
        if not readable:
            raise GitCustodyError("Git batch command exceeded its deadline")
        try:
            chunk = os.read(self.descriptor, 64 * 1024)
        except InterruptedError:
            return
        except OSError as error:
            raise GitCustodyError(f"cannot read Git batch output: {error}") from error
        if not chunk:
            raise GitCustodyError("Git batch command ended before returning a blob")
        self.buffer.extend(chunk)

    def read_line(self, *, maximum: int) -> bytes:
        while True:
            newline = self.buffer.find(b"\n")
            if newline >= 0:
                if newline > maximum:
                    raise GitCustodyError("Git returned an oversized batch header")
                result = bytes(self.buffer[:newline])
                del self.buffer[: newline + 1]
                return result
            if len(self.buffer) > maximum:
                raise GitCustodyError("Git returned an oversized batch header")
            self._fill()

    def read_exact(self, byte_count: int) -> bytes:
        while len(self.buffer) < byte_count:
            self._fill()
        result = bytes(self.buffer[:byte_count])
        del self.buffer[:byte_count]
        return result


class GitRepository:
    def __init__(self, path: Path) -> None:
        try:
            self.path = path.resolve(strict=True)
        except OSError as error:
            raise GitCustodyError(
                f"cannot resolve repository {path}: {error}"
            ) from error
        if not self.path.is_dir():
            raise GitCustodyError(f"repository {self.path} is not a directory")
        self._budget = GitExecutionBudget()
        probe = self._run(("rev-parse", "--git-dir"))
        if probe.returncode != 0:
            raise GitCustodyError(
                f"{self.path} is not an accessible Git repository: "
                f"{_stderr_summary(probe.stderr)}"
            )
        object_format = self._run(("rev-parse", "--show-object-format"))
        if object_format.returncode != 0:
            raise GitCustodyError(
                "Git cannot report the repository object format: "
                f"{_stderr_summary(object_format.stderr)}"
            )
        self.object_format = object_format.stdout.decode("ascii", "replace").strip()
        if self.object_format == "sha1":
            self.object_hex_length = 40
        elif self.object_format == "sha256":
            self.object_hex_length = 64
        else:
            raise GitCustodyError(
                f"unsupported Git object format {self.object_format!r}; "
                "expected sha1 or sha256"
            )

    @staticmethod
    def _environment() -> dict[str, str]:
        environment = {
            key: value
            for key, value in os.environ.items()
            if not key.startswith("GIT_")
        }
        environment.update(
            {
                "GIT_CONFIG_GLOBAL": os.devnull,
                "GIT_CONFIG_NOSYSTEM": "1",
                "GIT_NO_LAZY_FETCH": "1",
                "GIT_OPTIONAL_LOCKS": "0",
                "GIT_TERMINAL_PROMPT": "0",
                "LANG": "C",
                "LC_ALL": "C",
            }
        )
        return environment

    def _command(self, arguments: Sequence[str]) -> tuple[str, ...]:
        return (
            "git",
            "--no-pager",
            "--no-replace-objects",
            "-C",
            str(self.path),
            *arguments,
        )

    def _run(
        self, arguments: Sequence[str], *, input_data: bytes | None = None
    ) -> subprocess.CompletedProcess[bytes]:
        if input_data is not None:
            self._budget.charge_batch_input(len(input_data))
        timeout = self._budget.claim_command()
        try:
            return subprocess.run(
                self._command(arguments),
                input=input_data,
                stdout=subprocess.PIPE,
                stderr=subprocess.PIPE,
                timeout=timeout,
                check=False,
                env=self._environment(),
            )
        except FileNotFoundError as error:
            raise GitCustodyError("git executable was not found") from error
        except subprocess.TimeoutExpired as error:
            raise GitCustodyError(
                f"Git command exceeded its {timeout:.3f}-second deadline"
            ) from error
        except OSError as error:
            raise GitCustodyError(f"cannot execute Git: {error}") from error

    def _batch_check(self, object_specs: Sequence[str]) -> list[GitObjectInfo | None]:
        if not object_specs:
            return []
        payload = b"".join(spec.encode("utf-8") + b"\n" for spec in object_specs)
        result = self._run(
            ("cat-file", "--batch-check=%(objectname) %(objecttype) %(objectsize)"),
            input_data=payload,
        )
        if result.returncode != 0:
            raise GitCustodyError(
                "Git batch object resolution failed: "
                f"{_stderr_summary(result.stderr)}"
            )
        maximum_output = len(payload) + len(object_specs) * 160
        if len(result.stdout) > maximum_output:
            raise GitCustodyError("Git returned oversized batch object metadata")
        lines = result.stdout.splitlines()
        if len(lines) != len(object_specs):
            raise GitCustodyError(
                f"Git returned {len(lines)} batch metadata rows for "
                f"{len(object_specs)} requests"
            )
        resolved: list[GitObjectInfo | None] = []
        object_id_pattern = re.compile(
            rf"[0-9a-f]{{{self.object_hex_length}}}\Z".encode("ascii")
        )
        for line in lines:
            if line.endswith(b" missing"):
                resolved.append(None)
                continue
            fields = line.split(b" ")
            if len(fields) != 3 or object_id_pattern.fullmatch(fields[0]) is None:
                raise GitCustodyError("Git returned malformed batch object metadata")
            try:
                object_type = fields[1].decode("ascii")
                size = int(fields[2])
            except (UnicodeDecodeError, ValueError) as error:
                raise GitCustodyError(
                    "Git returned malformed batch object metadata"
                ) from error
            if size < 0:
                raise GitCustodyError("Git returned a negative object size")
            resolved.append(
                GitObjectInfo(
                    object_id=fields[0].decode("ascii"),
                    object_type=object_type,
                    size=size,
                )
            )
        return resolved

    def validate_commits(
        self,
        commit_locations: dict[str, list[str]],
        diagnostics: DiagnosticSink,
    ) -> set[str]:
        eligible: list[str] = []
        for commit, locations in commit_locations.items():
            if len(commit) != self.object_hex_length:
                for location in locations:
                    diagnostics.add(
                        location,
                        f"must be a full {self.object_hex_length}-hex "
                        f"{self.object_format} object ID for this repository",
                    )
            else:
                eligible.append(commit)

        valid: set[str] = set()
        for commit, info in zip(eligible, self._batch_check(eligible)):
            locations = commit_locations[commit]
            if info is None:
                for location in locations:
                    diagnostics.add(
                        location, f"commit object {commit} is not present locally"
                    )
            elif info.object_type != "commit":
                for location in locations:
                    diagnostics.add(
                        location,
                        f"{commit} names a {info.object_type!r} object, not a commit",
                    )
            else:
                valid.add(commit)
        return valid

    def resolve_blob_groups(
        self,
        references: dict[tuple[str, str], list[ReferenceUse]],
        valid_commits: set[str],
        diagnostics: DiagnosticSink,
    ) -> list[BlobGroup]:
        eligible = [key for key in references if key[0] in valid_commits]
        object_specs = [f"{commit}:{path}" for commit, path in eligible]
        groups: dict[str, BlobGroup] = {}
        metadata = self._batch_check(object_specs)
        for (commit, path), info in zip(eligible, metadata):
            uses = references[(commit, path)]
            if info is None:
                for use in uses:
                    diagnostics.add(
                        use.location,
                        f"path {path!r} is not present at commit {commit}",
                    )
                continue
            if info.object_type != "blob":
                for use in uses:
                    diagnostics.add(
                        use.location,
                        f"path {path!r} at commit {commit} is "
                        f"{info.object_type!r}, not a blob",
                    )
                continue
            if info.size > MAX_BLOB_BYTES:
                for use in uses:
                    diagnostics.add(
                        use.location,
                        f"blob is {info.size} bytes; verification limit is "
                        f"{MAX_BLOB_BYTES} bytes",
                    )
                continue
            group = groups.get(info.object_id)
            if group is None:
                groups[info.object_id] = BlobGroup(
                    object_id=info.object_id,
                    size=info.size,
                    uses=list(uses),
                )
            else:
                if group.size != info.size:
                    raise GitCustodyError(
                        f"Git reported inconsistent sizes for object {info.object_id}"
                    )
                group.uses.extend(uses)
        return list(groups.values())

    def verify_blob_groups(
        self, groups: Sequence[BlobGroup], diagnostics: DiagnosticSink
    ) -> None:
        if not groups:
            return
        request_bytes = sum(self.object_hex_length + 1 for _ in groups)
        self._budget.charge_batch_input(request_bytes)
        command_timeout = self._budget.claim_command()
        try:
            process = subprocess.Popen(
                self._command(("cat-file", "--batch")),
                stdin=subprocess.PIPE,
                stdout=subprocess.PIPE,
                stderr=subprocess.PIPE,
                bufsize=0,
                env=self._environment(),
            )
        except FileNotFoundError as error:
            raise GitCustodyError("git executable was not found") from error
        except OSError as error:
            raise GitCustodyError(f"cannot execute Git: {error}") from error

        assert process.stdin is not None
        assert process.stdout is not None
        assert process.stderr is not None
        deadline = time.monotonic() + command_timeout
        reader = _DeadlinePipeReader(process.stdout.fileno(), deadline)
        completed = False
        try:
            for group in groups:
                try:
                    process.stdin.write(group.object_id.encode("ascii") + b"\n")
                    process.stdin.flush()
                except (BrokenPipeError, OSError) as error:
                    raise GitCustodyError(
                        "Git batch process closed while receiving an object request"
                    ) from error
                header = reader.read_line(maximum=256)
                fields = header.split(b" ")
                if len(fields) != 3:
                    raise GitCustodyError("Git returned a malformed blob batch header")
                try:
                    returned_id = fields[0].decode("ascii")
                    object_type = fields[1].decode("ascii")
                    size = int(fields[2])
                except (UnicodeDecodeError, ValueError) as error:
                    raise GitCustodyError(
                        "Git returned a malformed blob batch header"
                    ) from error
                if (
                    returned_id != group.object_id
                    or object_type != "blob"
                    or size != group.size
                ):
                    raise GitCustodyError(
                        f"Git changed metadata while reading object {group.object_id}"
                    )
                data = reader.read_exact(size)
                if reader.read_exact(1) != b"\n":
                    raise GitCustodyError("Git omitted the blob batch terminator")
                _verify_group_bytes(group, data, diagnostics)
                del data

            try:
                process.stdin.close()
            except OSError as error:
                raise GitCustodyError("cannot close Git batch input") from error
            remaining = deadline - time.monotonic()
            if remaining <= 0:
                raise GitCustodyError("Git batch command exceeded its deadline")
            try:
                returncode = process.wait(timeout=remaining)
            except subprocess.TimeoutExpired as error:
                raise GitCustodyError(
                    "Git batch command exceeded its deadline"
                ) from error
            if returncode != 0:
                stderr = process.stderr.read(64 * 1024)
                raise GitCustodyError(
                    "Git batch blob read failed: " + _stderr_summary(stderr)
                )
            completed = True
        finally:
            if not completed and process.poll() is None:
                process.kill()
            if process.poll() is None:
                process.wait()
            for stream in (process.stdin, process.stdout, process.stderr):
                try:
                    stream.close()
                except OSError:
                    pass


def _stderr_summary(stderr: bytes) -> str:
    text = stderr.decode("utf-8", "replace").strip().replace("\n", " ")
    return text[:500] if text else "no Git diagnostic"


def git_blob_sha256(data: bytes) -> str:
    header = f"blob {len(data)}\0".encode("ascii")
    digest = hashlib.sha256()
    digest.update(header)
    digest.update(data)
    return digest.hexdigest()


def _verify_locator_once(
    data: bytes, needle: bytes, locations: Sequence[str], diagnostics: DiagnosticSink
) -> None:
    first = data.find(needle)
    if first < 0:
        for location in locations:
            diagnostics.add(
                location, "literal locator does not occur in the referenced blob"
            )
        return
    second = data.find(needle, first + 1)
    if second >= 0:
        for location in locations:
            diagnostics.add(
                location,
                "literal locator occurs more than once in the referenced blob",
            )


def _verify_group_bytes(
    group: BlobGroup, data: bytes, diagnostics: DiagnosticSink
) -> None:
    digest = git_blob_sha256(data)
    locator_locations: dict[bytes, list[str]] = {}
    for use in group.uses:
        if digest != use.expected_digest:
            diagnostics.add(
                f"{use.location}.git_blob_sha256",
                f"does not match Git-blob SHA-256 (expected {digest})",
            )
        for check in use.locators:
            locator_locations.setdefault(check.literal.encode("utf-8"), []).append(
                check.location
            )
    for needle, locations in locator_locations.items():
        _verify_locator_once(data, needle, locations, diagnostics)


def _preflight_blob_work(
    groups: Sequence[BlobGroup], diagnostics: DiagnosticSink
) -> bool:
    unique_blob_bytes = sum(group.size for group in groups)
    locator_bytes = 0
    locator_scan_bytes = 0
    for group in groups:
        needles = {
            check.literal.encode("utf-8")
            for use in group.uses
            for check in use.locators
        }
        locator_bytes += sum(len(needle) for needle in needles)
        # Exact-once checking performs at most two full-blob searches per needle.
        locator_scan_bytes += 2 * group.size * len(needles)

    allowed = True
    if unique_blob_bytes > MAX_TOTAL_UNIQUE_BLOB_BYTES:
        diagnostics.add(
            "$",
            f"references {unique_blob_bytes} unique Git blob bytes; limit is "
            f"{MAX_TOTAL_UNIQUE_BLOB_BYTES}",
        )
        allowed = False
    if locator_bytes > MAX_TOTAL_LOCATOR_BYTES:
        diagnostics.add(
            "$",
            f"requires {locator_bytes} distinct per-blob locator bytes; limit is "
            f"{MAX_TOTAL_LOCATOR_BYTES}",
        )
        allowed = False
    if locator_scan_bytes > MAX_TOTAL_LOCATOR_SCAN_BYTES:
        diagnostics.add(
            "$",
            f"requires up to {locator_scan_bytes} locator-scan bytes; limit is "
            f"{MAX_TOTAL_LOCATOR_SCAN_BYTES}",
        )
        allowed = False
    return allowed


def verify_repository_custody(
    repository_path: Path,
    commit_claims: Sequence[GitCommitClaim],
    blob_claims: Sequence[GitBlobClaim],
    diagnostics: DiagnosticSink,
) -> None:
    repository = GitRepository(repository_path)
    commit_locations: dict[str, list[str]] = {}
    for claim in commit_claims:
        commit_locations.setdefault(claim.commit, []).append(claim.location)

    references: dict[tuple[str, str], list[ReferenceUse]] = {}
    for claim in blob_claims:
        commit_locations.setdefault(claim.commit, []).append(claim.commit_location)
        references.setdefault((claim.commit, claim.path), []).append(
            ReferenceUse(
                location=claim.location,
                expected_digest=claim.expected_digest,
                locators=claim.locators,
            )
        )

    valid_commits = repository.validate_commits(commit_locations, diagnostics)
    groups = repository.resolve_blob_groups(references, valid_commits, diagnostics)
    if _preflight_blob_work(groups, diagnostics):
        repository.verify_blob_groups(groups, diagnostics)
