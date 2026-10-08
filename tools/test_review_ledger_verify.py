#!/usr/bin/env python3
"""Review-ledger schema tests and CLI/Git-custody tests with temporary repos."""

from __future__ import annotations

import copy
import hashlib
import json
import os
import shutil
import subprocess
import sys
import tempfile
import unittest
import uuid
from pathlib import Path
from typing import Any

if __package__:
    from . import review_ledger_git as GIT
    from . import review_ledger_verify as VERIFIER
else:
    import review_ledger_git as GIT
    import review_ledger_verify as VERIFIER


TOOLS = Path(__file__).resolve().parent
SCRIPT = TOOLS / "review_ledger_verify.py"
TESTDATA = TOOLS / "testdata" / "review-ledger"
EMPTY_BLOB_SHA256 = "473a0f4c3be8a93681a267e3b1e9a7dcda1185436fe141f7749120a303721813"


def git_blob_sha256(data: bytes) -> str:
    return hashlib.sha256(f"blob {len(data)}\0".encode("ascii") + data).hexdigest()


def locator(literal: str) -> dict[str, str]:
    return {
        "literal": literal,
        "literal_sha256": hashlib.sha256(literal.encode("utf-8")).hexdigest(),
    }


class _LedgerFixture:
    """Shared ledger shape; parser tests need no repository or subprocesses.

    These fixed IDs and fixture bytes describe syntax, not Git custody. The CLI
    test class overrides the IDs and blob reader with its temporary repository.
    """

    reviewed_commit = "1" * 40
    artifact_commit = "2" * 40
    evidence_commit = "3" * 40
    response_commit = "4" * 40

    @classmethod
    def blob_bytes(cls, commit: str, path: str) -> bytes:
        fixture_files = {
            "docs/review.md": "review.md",
            "docs/response.md": "response.md",
            "docs/source-proof.md": "source-proof.md",
            "tests/test_widget.py": "component-test.py",
            "evidence/installed-artifact.txt": "installed-artifact.txt",
            "evidence/live-store.txt": "live-store.txt",
            "evidence/evaluation.json": "evaluation.json",
        }
        return (TESTDATA / fixture_files[path]).read_bytes()

    @classmethod
    def evidence(
        cls,
        role: str,
        path: str,
        commit: str,
        literal: str,
        description: str | None = None,
    ) -> dict[str, Any]:
        return {
            "role": role,
            "path": path,
            "commit": commit,
            "git_blob_sha256": git_blob_sha256(cls.blob_bytes(commit, path)),
            "locator": locator(literal),
            "description": description or f"committed {role} evidence",
        }

    @classmethod
    def valid_ledger(cls) -> dict[str, Any]:
        review_path = "docs/review.md"
        return {
            "schema": "mneme.review-ledger.v1",
            "artifact": {
                "path": review_path,
                "artifact_commit": cls.artifact_commit,
                "git_blob_sha256": git_blob_sha256(
                    cls.blob_bytes(cls.artifact_commit, review_path)
                ),
            },
            "response": {
                "path": "docs/response.md",
                "response_commit": cls.response_commit,
                "git_blob_sha256": git_blob_sha256(
                    cls.blob_bytes(cls.response_commit, "docs/response.md")
                ),
            },
            "reviewed_commit": cls.reviewed_commit,
            "source_findings": [
                {
                    "id": "F1",
                    "locator": locator("### F1 — Unsafe widget input is accepted"),
                },
                {
                    "id": "F2",
                    "locator": locator("### F2 — Lease release remains synchronous"),
                },
            ],
            "entries": [
                {
                    "finding_id": "F1",
                    "disposition": "fixed",
                    "closure_tier": "component_test",
                    "summary": "The unsafe sentinel is rejected by the parser.",
                    "response_locator": locator(
                        "### Response F1 — Unsafe widget input"
                    ),
                    "evidence": [
                        cls.evidence(
                            "source_proof",
                            "docs/source-proof.md",
                            cls.reviewed_commit,
                            "The unsafe branch reaches the widget parser before "
                            "validation.",
                        ),
                        cls.evidence(
                            "component_test",
                            "tests/test_widget.py",
                            cls.evidence_commit,
                            "def test_rejects_unsafe_widget():",
                        ),
                    ],
                },
                {
                    "finding_id": "F2",
                    "disposition": "deferred",
                    "closure_tier": "source_proof",
                    "summary": "The synchronous boundary remains intentionally open.",
                    "response_locator": locator(
                        "### Response F2 — Synchronous lease release"
                    ),
                    "rationale": "Lease lifecycle work owns the larger boundary.",
                    "follow_up": {
                        "owner": "lease lifecycle goal",
                        "action": (
                            "Revisit after the asynchronous handoff contract lands."
                        ),
                    },
                    "evidence": [
                        cls.evidence(
                            "source_proof",
                            "docs/source-proof.md",
                            cls.reviewed_commit,
                            "The lease release path is currently synchronous by "
                            "construction.",
                        )
                    ],
                },
            ],
        }

    def install_role(self) -> dict[str, Any]:
        return self.evidence(
            "installed_artifact",
            "evidence/installed-artifact.txt",
            self.evidence_commit,
            "Installed artifact smoke recorded binary digest 0123456789abcdef.",
        )

    def live_role(self) -> dict[str, Any]:
        return self.evidence(
            "live_store",
            "evidence/live-store.txt",
            self.evidence_commit,
            "Read-only live-store smoke observed the expected widget rejection.",
        )

    def evaluation_role(self) -> dict[str, Any]:
        return self.evidence(
            "evaluation",
            "evidence/evaluation.json",
            self.evidence_commit,
            "evaluation comparator retained the simpler baseline",
        )


class ReviewLedgerSchemaTests(_LedgerFixture, unittest.TestCase):
    """Schema and disposition policy, independent of Git and CLI rendering."""

    def assert_schema_invalid(self, ledger: dict[str, Any], needle: str) -> None:
        diagnostics = VERIFIER.Diagnostics()
        VERIFIER.parse_ledger(ledger, diagnostics)
        self.assertGreater(diagnostics.total, 0)
        self.assertIn(needle, "\n".join(diagnostics.rendered()))

    def assert_schema_valid(self, ledger: dict[str, Any]) -> None:
        diagnostics = VERIFIER.Diagnostics()
        parsed = VERIFIER.parse_ledger(ledger, diagnostics)
        self.assertEqual(diagnostics.total, 0, diagnostics.rendered())
        self.assertIsInstance(parsed, VERIFIER.Ledger)

    def test_unknown_fields_are_rejected_at_every_object_level(self) -> None:
        mutations = [
            lambda ledger: ledger.update({"surprise": True}),
            lambda ledger: ledger["artifact"].update({"surprise": True}),
            lambda ledger: ledger["response"].update({"surprise": True}),
            lambda ledger: ledger["source_findings"][0].update({"surprise": True}),
            lambda ledger: ledger["source_findings"][0]["locator"].update(
                {"surprise": True}
            ),
            lambda ledger: ledger["entries"][0].update({"surprise": True}),
            lambda ledger: ledger["entries"][0]["response_locator"].update(
                {"surprise": True}
            ),
            lambda ledger: ledger["entries"][0]["evidence"][0].update(
                {"surprise": True}
            ),
            lambda ledger: ledger["entries"][1]["follow_up"].update(
                {"surprise": True}
            ),
        ]
        for mutate in mutations:
            with self.subTest(mutation=mutate):
                ledger = self.valid_ledger()
                mutate(ledger)
                self.assert_schema_invalid(ledger, "unknown field")

    def test_total_evidence_bound_is_checked_before_entry_materialization(self) -> None:
        ledger = self.valid_ledger()
        source_template = ledger["source_findings"][0]
        entry_template = ledger["entries"][1]
        source_findings = []
        entries = []
        for index in range(65):
            finding_id = f"F{index}"
            source = copy.deepcopy(source_template)
            source["id"] = finding_id
            source["locator"] = locator(f"source locator {index}")
            source_findings.append(source)

            entry = copy.deepcopy(entry_template)
            entry["finding_id"] = finding_id
            entry["response_locator"] = locator(f"response locator {index}")
            entry["evidence"] = [
                copy.deepcopy(entry_template["evidence"][0]) for _ in range(64)
            ]
            entries.append(entry)
        ledger["source_findings"] = source_findings
        ledger["entries"] = entries
        diagnostics = VERIFIER.Diagnostics()
        self.assertIsNone(VERIFIER.parse_ledger(ledger, diagnostics))
        self.assertIn(
            "more than 4096 evidence records", "\n".join(diagnostics.rendered())
        )

    def test_schema_and_enum_values_are_exact(self) -> None:
        ledger = self.valid_ledger()
        ledger["schema"] = "mneme.review-ledger.v2"
        self.assert_schema_invalid(ledger, "must be exactly 'mneme.review-ledger.v1'")

        ledger = self.valid_ledger()
        ledger["entries"][0]["disposition"] = "fixed / scoped"
        self.assert_schema_invalid(ledger, "must be one of")

        ledger = self.valid_ledger()
        ledger["entries"][0]["closure_tier"] = "test"
        self.assert_schema_invalid(ledger, "must be one of")

        ledger = self.valid_ledger()
        ledger["entries"][0]["evidence"][0]["role"] = "source"
        self.assert_schema_invalid(ledger, "must be one of")

    def test_source_and_entry_ids_are_unique_and_one_to_one(self) -> None:
        ledger = self.valid_ledger()
        ledger["source_findings"].append(copy.deepcopy(ledger["source_findings"][0]))
        self.assert_schema_invalid(ledger, "duplicates an earlier source finding ID")

        ledger = self.valid_ledger()
        ledger["entries"].append(copy.deepcopy(ledger["entries"][0]))
        self.assert_schema_invalid(ledger, "duplicates an earlier ledger entry")

        ledger = self.valid_ledger()
        ledger["entries"].pop()
        self.assert_schema_invalid(ledger, "missing entry for source finding 'F2'")

        ledger = self.valid_ledger()
        ledger["entries"][1]["finding_id"] = "F3"
        self.assert_schema_invalid(ledger, "has no matching source finding")

    def test_source_finding_locators_are_unique(self) -> None:
        ledger = self.valid_ledger()
        ledger["source_findings"][1]["locator"] = copy.deepcopy(
            ledger["source_findings"][0]["locator"]
        )
        self.assert_schema_invalid(ledger, "duplicates an earlier source finding locator")

    def test_response_locators_are_unique(self) -> None:
        ledger = self.valid_ledger()
        ledger["entries"][1]["response_locator"] = copy.deepcopy(
            ledger["entries"][0]["response_locator"]
        )
        self.assert_schema_invalid(ledger, "duplicates an earlier response locator")

    def test_all_commit_fields_require_full_repository_object_ids(self) -> None:
        ledger = self.valid_ledger()
        ledger["reviewed_commit"] = self.reviewed_commit[:12]
        self.assert_schema_invalid(ledger, "full 40- or 64-hex commit SHA")

        ledger = self.valid_ledger()
        ledger["artifact"]["artifact_commit"] = self.artifact_commit[:12]
        self.assert_schema_invalid(ledger, "full 40- or 64-hex commit SHA")

        ledger = self.valid_ledger()
        ledger["response"]["response_commit"] = self.response_commit[:12]
        self.assert_schema_invalid(ledger, "full 40- or 64-hex commit SHA")

        ledger = self.valid_ledger()
        ledger["entries"][0]["evidence"][0]["commit"] = self.reviewed_commit[:12]
        self.assert_schema_invalid(ledger, "full 40- or 64-hex commit SHA")

    def test_paths_must_be_normalized_repository_relative_paths(self) -> None:
        invalid_paths = (
            "/docs/review.md",
            "./docs/review.md",
            "docs/../docs/review.md",
            "docs//review.md",
            "docs\\review.md",
        )
        for path in invalid_paths:
            with self.subTest(path=path):
                ledger = self.valid_ledger()
                ledger["artifact"]["path"] = path
                self.assert_schema_invalid(ledger, "must")

    def test_every_entry_requires_source_proof_and_declared_tier_evidence(self) -> None:
        ledger = self.valid_ledger()
        ledger["entries"][0]["evidence"] = [
            ledger["entries"][0]["evidence"][1]
        ]
        self.assert_schema_invalid(ledger, "must include a source_proof record")

        ledger = self.valid_ledger()
        ledger["entries"][0]["closure_tier"] = "installed_artifact"
        self.assert_schema_invalid(ledger, "role matches closure_tier 'installed_artifact'")

    def test_fixed_and_already_fixed_require_component_test_evidence(self) -> None:
        for disposition in ("fixed", "already_fixed"):
            with self.subTest(disposition=disposition):
                ledger = self.valid_ledger()
                entry = ledger["entries"][0]
                entry["disposition"] = disposition
                entry["closure_tier"] = "installed_artifact"
                entry["evidence"] = [entry["evidence"][0], self.install_role()]
                self.assert_schema_invalid(ledger, "must include a component_test record")

        ledger = self.valid_ledger()
        ledger["entries"][0]["disposition"] = "already_fixed"
        self.assert_schema_valid(ledger)

    def test_runtime_tiers_are_orthogonal_and_require_the_exact_role(self) -> None:
        ledger = self.valid_ledger()
        entry = ledger["entries"][0]
        entry["closure_tier"] = "installed_artifact"
        entry["evidence"].append(self.install_role())
        self.assert_schema_valid(ledger)

        ledger = self.valid_ledger()
        entry = ledger["entries"][0]
        entry["closure_tier"] = "live_store"
        entry["evidence"].append(self.install_role())
        self.assert_schema_invalid(ledger, "role matches closure_tier 'live_store'")

    def test_confirmed_open_requires_follow_up_and_allows_extra_evidence(self) -> None:
        ledger = self.valid_ledger()
        entry = ledger["entries"][0]
        entry["disposition"] = "confirmed"
        entry["closure_tier"] = "source_proof"
        entry["follow_up"] = {"owner": "parser owner", "action": "Implement the fix."}
        self.assert_schema_valid(ledger)

        ledger = self.valid_ledger()
        entry = ledger["entries"][0]
        entry["disposition"] = "confirmed"
        entry["closure_tier"] = "source_proof"
        self.assert_schema_invalid(ledger, "requires owner and action")

        ledger = self.valid_ledger()
        entry = ledger["entries"][0]
        entry["disposition"] = "confirmed"
        entry["closure_tier"] = "component_test"
        entry["follow_up"] = {"owner": "parser owner", "action": "Implement the fix."}
        self.assert_schema_invalid(ledger, "must declare source_proof")

    def test_deferred_requires_reason_and_owned_follow_up(self) -> None:
        ledger = self.valid_ledger()
        del ledger["entries"][1]["rationale"]
        self.assert_schema_invalid(ledger, "requires a rationale")

        ledger = self.valid_ledger()
        del ledger["entries"][1]["follow_up"]
        self.assert_schema_invalid(ledger, "requires owner and action")

    def test_invalid_and_not_applied_require_rationale(self) -> None:
        for disposition, tier, role in (
            ("invalid", "evaluation", self.evaluation_role()),
            ("not_applied", "live_store", self.live_role()),
        ):
            with self.subTest(disposition=disposition):
                ledger = self.valid_ledger()
                entry = ledger["entries"][0]
                entry["disposition"] = disposition
                entry["closure_tier"] = tier
                entry["rationale"] = "The committed evidence supports this disposition."
                entry["evidence"].append(role)
                self.assert_schema_valid(ledger)

                del entry["rationale"]
                self.assert_schema_invalid(ledger, "requires a rationale")

    def test_superseded_runtime_tier_needs_component_test(self) -> None:
        ledger = self.valid_ledger()
        entry = ledger["entries"][0]
        entry["disposition"] = "superseded"
        entry["closure_tier"] = "installed_artifact"
        entry["rationale"] = "A stronger typed contract subsumes the proposed patch."
        entry["evidence"].append(self.install_role())
        self.assert_schema_valid(ledger)

        ledger = self.valid_ledger()
        entry = ledger["entries"][0]
        entry["disposition"] = "superseded"
        entry["closure_tier"] = "installed_artifact"
        entry["rationale"] = "A stronger typed contract subsumes the proposed patch."
        entry["evidence"] = [entry["evidence"][0], self.install_role()]
        self.assert_schema_invalid(ledger, "must include a component_test record")

        ledger = self.valid_ledger()
        entry = ledger["entries"][0]
        entry["disposition"] = "superseded"
        entry["closure_tier"] = "source_proof"
        entry["rationale"] = "A stronger source contract replaces the proposed API."
        entry["evidence"] = [entry["evidence"][0]]
        self.assert_schema_valid(ledger)

        ledger = self.valid_ledger()
        entry = ledger["entries"][0]
        entry["disposition"] = "superseded"
        del entry["summary"]
        self.assert_schema_invalid(ledger, "required field is missing")

    def test_closed_or_permanent_dispositions_forbid_follow_up(self) -> None:
        ledger = self.valid_ledger()
        ledger["entries"][0]["follow_up"] = {
            "owner": "nobody",
            "action": "This should be a separate finding.",
        }
        self.assert_schema_invalid(ledger, "is not permitted for disposition 'fixed'")

    def test_duplicate_evidence_records_are_rejected(self) -> None:
        ledger = self.valid_ledger()
        ledger["entries"][0]["evidence"].append(
            copy.deepcopy(ledger["entries"][0]["evidence"][0])
        )
        self.assert_schema_invalid(ledger, "duplicates an earlier evidence record")


class ReviewLedgerVerifyTests(_LedgerFixture, unittest.TestCase):
    """CLI results and Git custody against an actual temporary repository."""

    @classmethod
    def setUpClass(cls) -> None:
        cls.sandbox = tempfile.TemporaryDirectory(prefix="review-ledger-test-")
        cls.root = Path(cls.sandbox.name)
        cls.repo = cls.root / "repo"
        cls.repo.mkdir()
        cls._git("init", "--quiet")

        (cls.repo / "docs").mkdir()
        shutil.copyfile(TESTDATA / "source-proof.md", cls.repo / "docs/source-proof.md")
        cls._git("add", "docs/source-proof.md")
        cls._commit("reviewed source")
        cls.reviewed_commit = cls._git("rev-parse", "HEAD").stdout.strip()

        shutil.copyfile(TESTDATA / "review.md", cls.repo / "docs/review.md")
        cls._git("add", "docs/review.md")
        cls._commit("record independent review")
        cls.artifact_commit = cls._git("rev-parse", "HEAD").stdout.strip()

        (cls.repo / "tests").mkdir()
        (cls.repo / "evidence").mkdir()
        shutil.copyfile(
            TESTDATA / "component-test.py", cls.repo / "tests/test_widget.py"
        )
        for name in (
            "installed-artifact.txt",
            "live-store.txt",
            "evaluation.json",
            "duplicate-locator.txt",
        ):
            shutil.copyfile(TESTDATA / name, cls.repo / "evidence" / name)
        cls._git("add", "tests", "evidence")
        cls._commit("add closure evidence")
        cls.evidence_commit = cls._git("rev-parse", "HEAD").stdout.strip()

        shutil.copyfile(TESTDATA / "response.md", cls.repo / "docs/response.md")
        cls._git("add", "docs/response.md")
        cls._commit("record review response")
        cls.response_commit = cls._git("rev-parse", "HEAD").stdout.strip()

    @classmethod
    def tearDownClass(cls) -> None:
        cls.sandbox.cleanup()

    @classmethod
    def _git(cls, *arguments: str) -> subprocess.CompletedProcess[str]:
        return subprocess.run(
            ("git", "-C", str(cls.repo), *arguments),
            check=True,
            text=True,
            stdout=subprocess.PIPE,
            stderr=subprocess.PIPE,
        )

    @classmethod
    def _commit(cls, message: str) -> None:
        cls._git(
            "-c",
            "user.name=Review Ledger Test",
            "-c",
            "user.email=review-ledger@example.invalid",
            "commit",
            "--quiet",
            "-m",
            message,
        )

    @classmethod
    def blob_bytes(cls, commit: str, path: str) -> bytes:
        return subprocess.run(
            ("git", "-C", str(cls.repo), "cat-file", "blob", f"{commit}:{path}"),
            check=True,
            stdout=subprocess.PIPE,
            stderr=subprocess.PIPE,
        ).stdout

    def run_cli(
        self,
        ledger: dict[str, Any] | None = None,
        *,
        raw: str | None = None,
        json_mode: bool = False,
        repo: Path | None = None,
        ledger_path: Path | None = None,
        env: dict[str, str] | None = None,
    ) -> subprocess.CompletedProcess[str]:
        if ledger_path is None:
            ledger_path = self.root / f"ledger-{uuid.uuid4().hex}.json"
            if raw is not None:
                ledger_path.write_text(raw, encoding="utf-8")
            else:
                ledger_path.write_text(
                    json.dumps(ledger if ledger is not None else self.valid_ledger()),
                    encoding="utf-8",
                )
        command = [
            sys.executable,
            str(SCRIPT),
            "--repo",
            str(repo or self.repo),
            "--ledger",
            str(ledger_path),
        ]
        if json_mode:
            command.append("--json")
        environment = os.environ.copy()
        if env is not None:
            environment.update(env)
        return subprocess.run(
            command,
            text=True,
            stdout=subprocess.PIPE,
            stderr=subprocess.PIPE,
            check=False,
            env=environment,
            timeout=10,
        )

    def assert_invalid(self, ledger: dict[str, Any], needle: str) -> None:
        result = self.run_cli(ledger)
        self.assertEqual(result.returncode, 1, result.stdout + result.stderr)
        self.assertIn(needle, result.stderr)

    def test_valid_human_output_and_does_not_execute_test_evidence(self) -> None:
        sentinel = self.root / "evidence-executed"
        sentinel.unlink(missing_ok=True)
        result = self.run_cli(
            env={"REVIEW_LEDGER_EXECUTION_SENTINEL": str(sentinel)}
        )
        self.assertEqual(result.returncode, 0, result.stdout + result.stderr)
        self.assertIn("valid mneme.review-ledger.v1", result.stdout)
        self.assertEqual(result.stderr, "")
        self.assertFalse(sentinel.exists(), "the verifier executed committed evidence")

    def test_valid_json_output(self) -> None:
        result = self.run_cli(json_mode=True)
        self.assertEqual(result.returncode, 0, result.stdout + result.stderr)
        payload = json.loads(result.stdout)
        self.assertTrue(payload["valid"])
        self.assertEqual(payload["kind"], "verification")
        self.assertEqual(payload["finding_count"], 2)
        self.assertEqual(payload["entry_count"], 2)
        self.assertEqual(payload["evidence_count"], 3)
        self.assertEqual(payload["errors"], [])

    def test_duplicate_json_keys_are_an_operational_json_error(self) -> None:
        result = self.run_cli(
            raw='{"schema":"mneme.review-ledger.v1","schema":"mneme.review-ledger.v1"}'
        )
        self.assertEqual(result.returncode, 2)
        self.assertIn("duplicate JSON object key", result.stderr)

    def test_malformed_json_is_an_operational_error(self) -> None:
        result = self.run_cli(raw="{")
        self.assertEqual(result.returncode, 2)
        self.assertIn("strict JSON", result.stderr)

    def test_deep_json_is_a_stable_operational_error(self) -> None:
        result = self.run_cli(raw="[" * 5000 + "0" + "]" * 5000)
        self.assertEqual(result.returncode, 2, result.stdout + result.stderr)
        self.assertIn("JSON nesting exceeds the supported depth", result.stderr)

    def test_json_nesting_limit_preserves_schema_errors_at_the_boundary(self) -> None:
        for opening, closing in (("[", "]"), ('{"x":', "}")):
            for depth in (VERIFIER.MAX_JSON_DEPTH, VERIFIER.MAX_JSON_DEPTH + 1):
                with self.subTest(opening=opening, depth=depth):
                    result = self.run_cli(
                        raw=opening * depth + "0" + closing * depth,
                        json_mode=True,
                    )
                    payload = json.loads(result.stdout)
                    if depth <= VERIFIER.MAX_JSON_DEPTH:
                        self.assertEqual(result.returncode, 1, result.stdout)
                        self.assertEqual(payload["kind"], "verification")
                    else:
                        self.assertEqual(result.returncode, 2, result.stdout)
                        self.assertEqual(payload["kind"], "operational")
                        self.assertIn(
                            f"supported depth of {VERIFIER.MAX_JSON_DEPTH}",
                            payload["errors"][0],
                        )

    def test_json_depth_ignores_quoted_delimiters_and_escaped_quotes(self) -> None:
        ledger = self.valid_ledger()
        ledger["entries"][0]["summary"] = '[{"\\\\' * (VERIFIER.MAX_JSON_DEPTH + 1)
        result = self.run_cli(ledger)
        self.assertEqual(result.returncode, 0, result.stdout + result.stderr)

    def test_lone_unicode_surrogates_are_schema_errors_not_crashes(self) -> None:
        ledger = self.valid_ledger()
        ledger["entries"][0]["summary"] = "\ud800"
        result = self.run_cli(ledger)
        self.assertEqual(result.returncode, 1, result.stdout + result.stderr)
        self.assertIn("valid Unicode scalar text", result.stderr)

    def test_missing_ledger_is_an_io_error(self) -> None:
        missing = self.root / "does-not-exist.json"
        result = self.run_cli(ledger_path=missing)
        self.assertEqual(result.returncode, 2)
        self.assertIn("cannot open ledger", result.stderr)

    def test_ledger_reader_rejects_fifo_and_device_without_blocking(self) -> None:
        fifo = self.root / "ledger.fifo"
        os.mkfifo(fifo)
        result = self.run_cli(ledger_path=fifo)
        self.assertEqual(result.returncode, 2, result.stdout + result.stderr)
        self.assertIn("is not a regular file", result.stderr)

        device = Path("/dev/null")
        if device.exists():
            result = self.run_cli(ledger_path=device)
            self.assertEqual(result.returncode, 2, result.stdout + result.stderr)
            self.assertIn("is not a regular file", result.stderr)

    def test_non_repository_is_a_git_operational_error(self) -> None:
        not_repo = self.root / "not-a-repo"
        not_repo.mkdir(exist_ok=True)
        result = self.run_cli(repo=not_repo, json_mode=True)
        self.assertEqual(result.returncode, 2)
        payload = json.loads(result.stdout)
        self.assertEqual(payload["kind"], "operational")
        self.assertFalse(payload["valid"])

    def test_inherited_git_environment_cannot_redirect_repo_or_config(self) -> None:
        result = self.run_cli(
            env={
                "GIT_DIR": str(self.root / "poison.git"),
                "GIT_WORK_TREE": str(self.root / "poison-worktree"),
                "GIT_OBJECT_DIRECTORY": str(self.root / "poison-objects"),
                "GIT_ALTERNATE_OBJECT_DIRECTORIES": str(
                    self.root / "poison-alternates"
                ),
                "GIT_CONFIG_COUNT": "1",
                "GIT_CONFIG_KEY_0": "core.repositoryformatversion",
                "GIT_CONFIG_VALUE_0": "999",
                "GIT_CONFIG_GLOBAL": str(self.root / "poison-config"),
            }
        )
        self.assertEqual(result.returncode, 0, result.stdout + result.stderr)

    def test_git_execution_budget_caps_commands_deadline_and_batch_input(self) -> None:
        ticks = iter((10.0, 10.0, 10.0))
        budget = GIT.GitExecutionBudget(
            max_commands=1,
            total_seconds=5.0,
            clock=lambda: next(ticks),
        )
        self.assertEqual(budget.claim_command(), 5.0)
        with self.assertRaisesRegex(GIT.GitCustodyError, "command budget"):
            budget.claim_command()

        ticks = iter((20.0, 21.0))
        expired = GIT.GitExecutionBudget(
            max_commands=1,
            total_seconds=0.5,
            clock=lambda: next(ticks),
        )
        with self.assertRaisesRegex(GIT.GitCustodyError, "global deadline"):
            expired.claim_command()

        control = GIT.GitExecutionBudget()
        with self.assertRaisesRegex(GIT.GitCustodyError, "control input"):
            control.charge_batch_input(GIT.MAX_GIT_BATCH_INPUT_BYTES + 1)

    def test_blob_work_preflight_enforces_aggregate_limits(self) -> None:
        diagnostics = VERIFIER.Diagnostics()
        too_many_bytes = [
            GIT.BlobGroup(
                object_id=f"{index:040x}",
                size=GIT.MAX_BLOB_BYTES,
                uses=[],
            )
            for index in range(5)
        ]
        self.assertFalse(GIT._preflight_blob_work(too_many_bytes, diagnostics))
        self.assertIn("unique Git blob bytes", "\n".join(diagnostics.rendered()))

        checks = tuple(
            GIT.LocatorCheck(
                literal=f"distinct locator {index}",
                location=f"locator[{index}]",
            )
            for index in range(5)
        )
        scan_heavy = GIT.BlobGroup(
            object_id="f" * 40,
            size=GIT.MAX_BLOB_BYTES,
            uses=[
                GIT.ReferenceUse(
                    location="evidence",
                    expected_digest="0" * 64,
                    locators=checks,
                )
            ],
        )
        diagnostics = VERIFIER.Diagnostics()
        self.assertFalse(GIT._preflight_blob_work([scan_heavy], diagnostics))
        self.assertIn("locator-scan bytes", "\n".join(diagnostics.rendered()))

    def test_distinct_references_to_same_git_object_are_grouped(self) -> None:
        digest = git_blob_sha256(
            self.blob_bytes(self.reviewed_commit, "docs/source-proof.md")
        )
        check = GIT.LocatorCheck(
            literal=(
                "The unsafe branch reaches the widget parser before validation."
            ),
            location="locator",
        )
        use = GIT.ReferenceUse(
            location="evidence", expected_digest=digest, locators=(check,)
        )
        references = {
            (self.reviewed_commit, "docs/source-proof.md"): [use],
            (self.evidence_commit, "docs/source-proof.md"): [use],
        }
        repository = GIT.GitRepository(self.repo)
        diagnostics = VERIFIER.Diagnostics()
        valid = repository.validate_commits(
            {
                self.reviewed_commit: ["reviewed"],
                self.evidence_commit: ["evidence"],
            },
            diagnostics,
        )
        groups = repository.resolve_blob_groups(references, valid, diagnostics)
        self.assertEqual(diagnostics.total, 0, diagnostics.rendered())
        self.assertEqual(len(groups), 1)
        self.assertEqual(len(groups[0].uses), 2)

    def test_git_blob_sha256_has_a_fixed_independent_vector(self) -> None:
        self.assertEqual(git_blob_sha256(b""), EMPTY_BLOB_SHA256)

    def test_review_response_and_reviewed_commits_have_distinct_jobs(self) -> None:
        self.assertEqual(
            len({self.reviewed_commit, self.artifact_commit, self.response_commit}), 3
        )
        ledger = self.valid_ledger()
        ledger["artifact"]["artifact_commit"] = self.reviewed_commit
        self.assert_invalid(ledger, "path 'docs/review.md' is not present")

        ledger = self.valid_ledger()
        ledger["response"]["response_commit"] = self.artifact_commit
        self.assert_invalid(ledger, "path 'docs/response.md' is not present")

        ledger = self.valid_ledger()
        ledger["reviewed_commit"] = "0" * 40
        self.assert_invalid(ledger, "commit object")

    def test_sha256_repository_accepts_full_64_hex_commits(self) -> None:
        with tempfile.TemporaryDirectory(dir=self.root) as temporary:
            repo = Path(temporary) / "sha256-repo"
            init = subprocess.run(
                ("git", "init", "--quiet", "--object-format=sha256", str(repo)),
                text=True,
                stdout=subprocess.PIPE,
                stderr=subprocess.PIPE,
                check=False,
            )
            if init.returncode != 0:
                self.skipTest("installed Git does not support SHA-256 repositories")
            (repo / "review.md").write_text(
                "### F1 — One committed finding\n", encoding="utf-8"
            )
            subprocess.run(
                ("git", "-C", str(repo), "add", "review.md"), check=True
            )
            subprocess.run(
                (
                    "git",
                    "-C",
                    str(repo),
                    "-c",
                    "user.name=Review Ledger Test",
                    "-c",
                    "user.email=review-ledger@example.invalid",
                    "commit",
                    "--quiet",
                    "-m",
                    "review",
                ),
                check=True,
            )
            commit = subprocess.run(
                ("git", "-C", str(repo), "rev-parse", "HEAD"),
                check=True,
                text=True,
                stdout=subprocess.PIPE,
            ).stdout.strip()
            self.assertEqual(len(commit), 64)
            empty_blob_id = subprocess.run(
                ("git", "-C", str(repo), "hash-object", "--stdin"),
                check=True,
                input=b"",
                stdout=subprocess.PIPE,
                stderr=subprocess.PIPE,
            ).stdout.decode("ascii").strip()
            self.assertEqual(empty_blob_id, EMPTY_BLOB_SHA256)
            blob = (repo / "review.md").read_bytes()
            literal = "### F1 — One committed finding"
            ledger = {
                "schema": "mneme.review-ledger.v1",
                "artifact": {
                    "path": "review.md",
                    "artifact_commit": commit,
                    "git_blob_sha256": git_blob_sha256(blob),
                },
                "response": {
                    "path": "review.md",
                    "response_commit": commit,
                    "git_blob_sha256": git_blob_sha256(blob),
                },
                "reviewed_commit": commit,
                "source_findings": [{"id": "F1", "locator": locator(literal)}],
                "entries": [
                    {
                        "finding_id": "F1",
                        "disposition": "confirmed",
                        "closure_tier": "source_proof",
                        "summary": "The finding remains open.",
                        "response_locator": locator(literal),
                        "follow_up": {
                            "owner": "test owner",
                            "action": "Resolve the finding.",
                        },
                        "evidence": [
                            {
                                "role": "source_proof",
                                "path": "review.md",
                                "commit": commit,
                                "git_blob_sha256": git_blob_sha256(blob),
                                "locator": locator(literal),
                                "description": "Committed source proof.",
                            }
                        ],
                    }
                ],
            }
            result = self.run_cli(ledger, repo=repo)
            self.assertEqual(result.returncode, 0, result.stdout + result.stderr)

    def test_git_blob_sha256_binds_artifact_and_evidence_bytes(self) -> None:
        ledger = self.valid_ledger()
        ledger["artifact"]["git_blob_sha256"] = "0" * 64
        self.assert_invalid(ledger, "does not match Git-blob SHA-256")

        ledger = self.valid_ledger()
        ledger["response"]["git_blob_sha256"] = "0" * 64
        self.assert_invalid(ledger, "does not match Git-blob SHA-256")

        ledger = self.valid_ledger()
        ledger["entries"][0]["evidence"][0]["git_blob_sha256"] = "f" * 64
        self.assert_invalid(ledger, "does not match Git-blob SHA-256")

    def test_locator_hashes_bind_the_exact_utf8_literal(self) -> None:
        ledger = self.valid_ledger()
        ledger["source_findings"][0]["locator"]["literal_sha256"] = "0" * 64
        self.assert_invalid(ledger, "does not match the locator literal")

        ledger = self.valid_ledger()
        ledger["entries"][0]["response_locator"]["literal_sha256"] = "0" * 64
        self.assert_invalid(ledger, "does not match the locator literal")

        ledger = self.valid_ledger()
        ledger["entries"][0]["evidence"][0]["locator"][
            "literal_sha256"
        ] = "f" * 64
        self.assert_invalid(ledger, "does not match the locator literal")

    def test_literal_locators_must_exist_exactly_once_in_the_bound_blob(self) -> None:
        ledger = self.valid_ledger()
        ledger["source_findings"][0]["locator"] = locator("not in the review")
        self.assert_invalid(ledger, "does not occur")

        ledger = self.valid_ledger()
        ledger["entries"][0]["response_locator"] = locator("not in the response")
        self.assert_invalid(ledger, "does not occur")

        ledger = self.valid_ledger()
        duplicate = self.evidence(
            "source_proof",
            "evidence/duplicate-locator.txt",
            self.evidence_commit,
            "aaa",
        )
        ledger["entries"][0]["evidence"][0] = duplicate
        self.assert_invalid(ledger, "occurs more than once")

    def test_missing_path_and_non_blob_path_are_evidence_failures(self) -> None:
        ledger = self.valid_ledger()
        ledger["entries"][0]["evidence"][0]["path"] = "does/not/exist"
        self.assert_invalid(ledger, "is not present")

        ledger = self.valid_ledger()
        ledger["entries"][0]["evidence"][1]["path"] = "tests"
        self.assert_invalid(ledger, "not a blob")

    def test_diagnostics_are_bounded_and_report_the_omitted_count(self) -> None:
        ledger = self.valid_ledger()
        ledger.update({f"unknown_{index}": index for index in range(75)})
        result = self.run_cli(ledger, json_mode=True)
        self.assertEqual(result.returncode, 1)
        payload = json.loads(result.stdout)
        self.assertEqual(payload["error_count"], 75)
        self.assertEqual(len(payload["errors"]), 51)
        self.assertIn("additional error(s) omitted", payload["errors"][-1])


if __name__ == "__main__":
    unittest.main()
