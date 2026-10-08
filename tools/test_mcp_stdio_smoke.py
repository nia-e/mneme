"""Focused contract tests for the current stdio smoke harness."""

import sys
import unittest
from pathlib import Path
from unittest.mock import patch

sys.path.insert(0, str(Path(__file__).parent))
import mcp_stdio_smoke as smoke


def context():
    return {
        "schema": "mneme.context.v7",
        "core": [{"id": "core"}],
        "primary": [{"id": "primary"}],
        "expansions": [],
        "episodes": [{"id": "episode"}],
        "retrieval": {"partial": False, "lanes": {"primary": {"seed_coverage": None}}},
        "episodic_retrieval": {"state": "searched"},
        "partial": False,
    }


class StdioSmokeContractTests(unittest.TestCase):
    def test_current_context_has_four_arrays_and_one_retrieval_lane(self):
        self.assertEqual(smoke.context_summary(context()), {
            "schema": "mneme.context.v7", "core": 1, "primary": 1,
            "expansions": 0, "episodes": 1, "episodic_state": "searched",
            "presentation_partial": False, "retrieval_partial": False,
        })

    def test_old_or_empty_compatibility_lanes_are_refused(self):
        for schema in ("mneme.context.v4", "mneme.context.v5", "mneme.context.v6",
                       "mneme.context.v8"):
            with self.subTest(schema=schema):
                old = context()
                old["schema"] = schema
                with self.assertRaisesRegex(RuntimeError, "unexpected context schema"):
                    smoke.context_summary(old)
        for change in (
            lambda c: c.update(probationary=[]),
            lambda c: c["retrieval"]["lanes"].update(probationary={"seed_coverage": None}),
            lambda c: c.update(omitted={"probationary": 0}),
        ):
            with self.subTest(change=change):
                value = context()
                change(value)
                with self.assertRaises(RuntimeError):
                    smoke.context_summary(value)

    def test_catalogs_exactly_match_native_profile_contracts(self):
        # Mirrors golden_tool_catalogs_match_capability_profiles in the MCP
        # server, independently of the smoke's inventory and set composition.
        # Keep exact equality: an unexpected tool is not an acceptable superset.
        catalogs = {
            "read-only": """databases activity database_control status query
                recall_context recall episode concern get list graph neighbors
                remote_edges core contradictions merges walk""",
            "receipt-grounded": """databases activity database_control status query
                recall_context recall episode concern get list graph neighbors
                remote_edges core contradictions merges walk reflect""",
            "curator": """databases activity database_control status query
                recall_context recall episode concern get list graph neighbors
                remote_edges core ingest save capture retag link contradict
                contradictions merges walk reflect""",
            "operator": """databases activity database_control snapshot_create
                status decay prune query recall_context recall episode concern
                get list graph neighbors remote_edges core ingest save capture retag
                edit_body edit_summary forget
                link supersede contradict contradictions reconcile merges merge
                walk reflect""",
        }
        self.assertEqual(set(catalogs), set(smoke.PROFILE_CHOICES))
        for profile, names in catalogs.items():
            with self.subTest(profile=profile):
                self.assertEqual(smoke.expected_catalog(profile), set(names.split()))

    def test_default_profile_hides_save_and_raw_denial_precedes_checkout(self):
        argv = ["smoke", "--user-db", "/tmp/user.db", "--project-db", "/tmp/project.db"]
        with patch.object(sys, "argv", argv):
            self.assertEqual(smoke.parse_args().capability_profile, "receipt-grounded")
        for profile in ("read-only", "receipt-grounded"):
            with self.subTest(profile=profile):
                name, arguments, boundary = smoke.denied_probe(profile)
                self.assertEqual(name, "save")
                self.assertNotIn(name, smoke.expected_catalog(profile))
                self.assertEqual(arguments, {
                    "db": "does-not-exist", "kind": "note", "summary": "capability probe",
                    "operation_id": "stdio-capability-probe",
                })
                self.assertEqual(boundary, "capability profile")

    def test_curator_raw_summary_edit_denial_is_complete_before_checkout(self):
        name, arguments, boundary = smoke.denied_probe("curator")
        self.assertEqual(name, "edit_summary")
        self.assertNotIn(name, smoke.expected_catalog("curator"))
        self.assertIn(name, smoke.expected_catalog("operator"))
        self.assertEqual(arguments, {
            "db": "does-not-exist",
            "expected_db_id": "00000000000000000000000000",
            "id": "00000000000000000000000000",
            "expected_snapshot_sha256": "a" * 64,
            "summary": "capability probe",
        })
        self.assertEqual(boundary, "capability profile")

    def test_removed_probation_expectation_flag_is_not_accepted(self):
        argv = ["smoke", "--user-db", "/tmp/user.db", "--project-db", "/tmp/project.db",
                "--expect-user-probationary", "0"]
        with patch.object(sys, "argv", argv), self.assertRaises(SystemExit) as raised:
            smoke.parse_args()
        self.assertEqual(raised.exception.code, 2)


if __name__ == "__main__":
    unittest.main()
