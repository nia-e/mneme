#!/usr/bin/env python3
"""Focused contract tests for change_gate_select.py."""

from __future__ import annotations

import json
from pathlib import Path
import subprocess
import sys
import unittest


TOOLS = Path(__file__).resolve().parent
SCRIPT = TOOLS / "change_gate_select.py"
sys.path.insert(0, str(TOOLS))

import change_gate_select as SELECTOR  # noqa: E402


class ChangeGateSelectTests(unittest.TestCase):
    def run_cli(self, *arguments: str) -> subprocess.CompletedProcess[str]:
        return subprocess.run(
            (sys.executable, str(SCRIPT), *arguments),
            check=False,
            text=True,
            stdout=subprocess.PIPE,
            stderr=subprocess.PIPE,
            timeout=10,
        )

    def run_json(self, *arguments: str) -> tuple[subprocess.CompletedProcess[str], dict]:
        result = self.run_cli(*arguments, "--json")
        return result, json.loads(result.stdout)

    def test_public_interface_selects_only_its_pack(self) -> None:
        result, report = self.run_json(
            "--class",
            "public-interface",
            "--path",
            "crates/mneme-mcp/src/response.rs",
        )

        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertEqual(report["status"], "selected")
        self.assertEqual(report["declared_classes"], ["public-interface"])
        self.assertEqual(
            [pack["operation_class"] for pack in report["selected_packs"]],
            ["public-interface"],
        )
        self.assertEqual(
            report["selected_packs"][0]["reference"],
            ".agents/skills/mneme-change-gates/references/public-interface.md",
        )
        self.assertEqual(report["required_evidence_floor"], "component_test")
        self.assertEqual(report["omissions"], [])

    def test_composite_output_is_canonical_and_uses_strongest_evidence_tier(self) -> None:
        first = self.run_cli(
            "--class",
            "transport-package",
            "--class",
            "storage-admission",
            "--class",
            "public-interface",
            "--path",
            "crates/mneme-cozo/src/cozo_store/opening.rs",
            "--path",
            "crates/mneme-mcp/src/http.rs",
            "--path",
            "crates/mneme-mcp/src/response.rs",
            "--json",
        )
        second = self.run_cli(
            "--path",
            "crates/mneme-mcp/src/response.rs",
            "--class",
            "public-interface",
            "--path",
            "crates/mneme-mcp/src/http.rs",
            "--class",
            "storage-admission",
            "--path",
            "crates/mneme-cozo/src/cozo_store/opening.rs",
            "--class",
            "transport-package",
            "--json",
        )

        self.assertEqual(first.returncode, 0, first.stderr)
        self.assertEqual(second.returncode, 0, second.stderr)
        self.assertEqual(first.stdout, second.stdout)
        report = json.loads(first.stdout)
        self.assertEqual(
            report["declared_classes"],
            ["public-interface", "transport-package", "storage-admission"],
        )
        self.assertEqual(report["required_evidence_floor"], "installed_artifact")
        self.assertEqual(report["downstream_routing"], [])

    def test_sensitive_path_never_auto_grants_an_omitted_class(self) -> None:
        result, report = self.run_json(
            "--class",
            "storage-admission",
            "--path",
            "crates/mneme-store-path/src/fresh.rs",
        )

        self.assertEqual(result.returncode, 2)
        self.assertEqual(report["status"], "error")
        self.assertEqual(
            [pack["operation_class"] for pack in report["selected_packs"]],
            ["storage-admission"],
        )
        self.assertEqual(report["omissions"], ["storage-publication"])
        self.assertIn(
            "declaration_conflict", {error["code"] for error in report["errors"]}
        )

    def test_ambiguous_path_requires_a_matching_explicit_declaration(self) -> None:
        result, report = self.run_json(
            "--class",
            "capability",
            "--path",
            "crates/mneme-store-path/src/lib.rs",
        )

        self.assertEqual(result.returncode, 2)
        error = next(
            error
            for error in report["errors"]
            if error["code"] == "ambiguous_path_authority"
        )
        self.assertEqual(
            error["classes"], ["storage-admission", "storage-publication"]
        )
        self.assertEqual(
            [pack["operation_class"] for pack in report["selected_packs"]],
            ["capability"],
        )

    def test_conflicting_public_surface_declaration_fails_closed(self) -> None:
        result, report = self.run_json(
            "--class",
            "capability",
            "--path",
            "crates/mneme-mcp/src/response.rs",
        )

        self.assertEqual(result.returncode, 2)
        self.assertEqual(report["omissions"], ["public-interface"])
        self.assertEqual(report["errors"][0]["code"], "declaration_conflict")

    def test_unclassified_path_requires_recorded_semantic_attestation(self) -> None:
        pending, pending_report = self.run_json(
            "--class",
            "capability",
            "--path",
            "crates/mneme-engine/src/deleted-capability-module.rs",
        )

        self.assertEqual(pending.returncode, 2)
        self.assertEqual(pending_report["status"], "needs-attestation")
        self.assertEqual(
            pending_report["unclassified_paths"],
            ["crates/mneme-engine/src/deleted-capability-module.rs"],
        )
        self.assertEqual(
            [item["path"] for item in pending_report["pending_attestations"]],
            ["crates/mneme-engine/src/deleted-capability-module.rs"],
        )

        selected, selected_report = self.run_json(
            "--class",
            "capability",
            "--path",
            "crates/mneme-engine/src/deleted-capability-module.rs",
            "--attest-path",
            "crates/mneme-engine/src/deleted-capability-module.rs",
            "deleted sibling test exercises capability denial only",
        )

        self.assertEqual(selected.returncode, 0, selected.stderr)
        self.assertEqual(selected_report["status"], "selected")
        self.assertEqual(selected_report["pending_attestations"], [])

    def test_all_classes_are_additive_for_composite_changes(self) -> None:
        arguments: list[str] = []
        for operation_class in reversed(tuple(SELECTOR.CLASS_ORDER)):
            arguments.extend(("--class", operation_class))
        arguments.extend(
            (
                "--path",
                "crates/mneme-mcp/src/main.rs",
                "--attest-path",
                "crates/mneme-mcp/src/main.rs",
                "all six entry-point boundaries are intentionally changed",
            )
        )

        result, report = self.run_json(*arguments)

        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertEqual(report["declared_classes"], list(SELECTOR.CLASS_ORDER))
        self.assertEqual(len(report["selected_packs"]), 6)
        self.assertEqual(len(report["required_gates"]), 34)

    def test_multiplexed_entry_point_needs_a_semantic_attestation(self) -> None:
        pending, pending_report = self.run_json(
            "--class",
            "public-interface",
            "--path",
            "crates/mneme-mcp/src/main.rs",
        )

        self.assertEqual(pending.returncode, 2)
        self.assertEqual(pending_report["status"], "needs-attestation")

        selected, selected_report = self.run_json(
            "--class",
            "public-interface",
            "--path",
            "crates/mneme-mcp/src/main.rs",
            "--attest-path",
            "crates/mneme-mcp/src/main.rs",
            "the diff changes catalog rendering only",
        )

        self.assertEqual(selected.returncode, 0, selected.stderr)
        self.assertEqual(selected_report["status"], "selected")
        self.assertEqual(
            selected_report["path_attestations"],
            [
                {
                    "path": "crates/mneme-mcp/src/main.rs",
                    "rationale": "the diff changes catalog rendering only",
                }
            ],
        )

    def test_repeated_inputs_are_deduplicated(self) -> None:
        result, report = self.run_json(
            "--class",
            "public-interface",
            "--class",
            "public-interface",
            "--path",
            "./crates/mneme-mcp/src/response.rs",
            "--path",
            "crates/mneme-mcp/src/response.rs",
        )

        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertEqual(report["declared_classes"], ["public-interface"])
        self.assertEqual(
            report["touched_paths"], ["crates/mneme-mcp/src/response.rs"]
        )

    def test_unknown_class_and_unsafe_paths_fail_as_json(self) -> None:
        result, report = self.run_json(
            "--class",
            "storage",
            "--path",
            "/tmp/memory.db",
            "--path",
            "../memory.db",
        )

        self.assertEqual(result.returncode, 2)
        self.assertEqual(
            {error["code"] for error in report["errors"]},
            {"unknown_class", "invalid_path"},
        )
        self.assertEqual(report["selected_packs"], [])

    def test_replaced_router_skills_are_not_emitted_as_phantom_routes(self) -> None:
        result, report = self.run_json(
            "--class",
            "public-interface",
            "--class",
            "storage-admission",
            "--class",
            "storage-publication",
            "--path",
            "crates/mneme-mcp/src/response.rs",
            "--path",
            "crates/mneme-cozo/src/cozo_store/opening.rs",
            "--path",
            "crates/mneme-store-path/src/fresh.rs",
        )

        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertEqual(report["downstream_routing"], [])

    def test_native_bootstrap_route_matches_historical_forward_audit(self) -> None:
        result, report = self.run_json(
            "--class",
            "public-interface",
            "--class",
            "storage-admission",
            "--class",
            "storage-publication",
            "--path",
            "crates/mneme-cozo/src/cozo_store/fresh_current.rs",
            "--path",
            "crates/mnemed/src/bootstrap.rs",
            "--path",
            "crates/mnemed/tests/bootstrap_create_lifecycle.rs",
            "--attest-path",
            "crates/mnemed/src/bootstrap.rs",
            "current-schema CLI materialization with retained lease authority",
            "--attest-path",
            "crates/mnemed/tests/bootstrap_create_lifecycle.rs",
            "regression coverage for the declared bootstrap boundaries",
        )

        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertEqual(
            report["declared_classes"],
            ["public-interface", "storage-admission", "storage-publication"],
        )
        self.assertNotIn(
            "stale-writer-compatibility", report["declared_classes"]
        )
        self.assertEqual(report["required_evidence_floor"], "component_test")

    def test_vendored_internal_change_does_not_imply_a_package_claim(self) -> None:
        pending, pending_report = self.run_json(
            "--class",
            "storage-admission",
            "--path",
            "vendor/mnestic/src/storage/sqlite.rs",
        )

        self.assertEqual(pending.returncode, 2)
        self.assertEqual(pending_report["status"], "needs-attestation")
        self.assertEqual(pending_report["omissions"], [])

        selected, selected_report = self.run_json(
            "--class",
            "storage-admission",
            "--path",
            "vendor/mnestic/src/storage/sqlite.rs",
            "--attest-path",
            "vendor/mnestic/src/storage/sqlite.rs",
            "the diff extends only the bounded private snapshot reader",
        )

        self.assertEqual(selected.returncode, 0, selected.stderr)
        self.assertEqual(selected_report["status"], "selected")
        self.assertEqual(
            selected_report["declared_classes"], ["storage-admission"]
        )
        self.assertEqual(selected_report["required_evidence_floor"], "component_test")

    def test_human_success_and_failure_use_separate_streams(self) -> None:
        success = self.run_cli(
            "--class",
            "transport-package",
            "--path",
            "crates/mneme-mcp/src/http.rs",
        )
        failure = self.run_cli()

        self.assertEqual(success.returncode, 0)
        self.assertEqual(success.stderr, "")
        self.assertIn("status: selected\n", success.stdout)
        self.assertIn("DIST-01", success.stdout)
        self.assertEqual(failure.returncode, 2)
        self.assertEqual(failure.stdout, "")
        self.assertIn("[missing_class]", failure.stderr)
        self.assertIn("[missing_path]", failure.stderr)


if __name__ == "__main__":
    unittest.main()
