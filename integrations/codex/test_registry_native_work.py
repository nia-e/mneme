"""Aggregate decoded work across identity and exact association owner reads."""
import unittest
import time
from unittest.mock import patch

import hook_recall as recall
from fixture_recall_owner import ID1, FakeClient, OverlapFixture
from librarian_policy import LibrarianBudget


class RegistryNativeWorkTests(unittest.TestCase, OverlapFixture):
    def setUp(self):
        OverlapFixture.__init__(self, self)
        self.budget = LibrarianBudget(effort="low")
        self.target = recall.AssociationTarget("overlap001", ID1, "Evidence one", "semantic")

    def client(self):
        return FakeClient(self.catalog, self.context, self.nodes)

    def identity(self, **kwargs):
        return recall.resolve_project_identity(self.config, self.root, budget=self.budget, **kwargs)

    def association(self, **kwargs):
        return recall.check_association_target(self.config, self.root, self.target,
            expected_db_id=self.db_id, timeout=1, budget=self.budget, **kwargs)

    def test_reader_exhaustion_refuses_before_transport_construction(self):
        shared = {"deadline": time.monotonic() + self.budget.native_seconds,
                  "decoded_bytes": self.budget.native_read_bytes}
        with patch("hook_recall._service_client") as client:
            result = recall.collect_reader(self.config, "Inspect current owner evidence", self.root,
                                           budget=self.budget, shared_work=shared)
        client.assert_not_called()
        self.assertEqual(result["outcome"], "timeout")
        self.assertEqual(result["native_work"]["decoded_bytes"], self.budget.native_read_bytes)

    def test_reader_failed_response_fences_shared_work_without_fabricated_usage(self):
        client = self.client()
        original = client.call_tool
        def failing(name, args):
            if name == "recall_context":
                raise OSError("connection failed after partial native output")
            return original(name, args)
        client.call_tool = failing
        shared = {"deadline": time.monotonic() + self.budget.native_seconds, "decoded_bytes": 71}
        with patch("hook_recall.McpClient", return_value=client):
            result = recall.collect_reader(self.config, "Investigate aggregate read work", self.root,
                                           budget=self.budget, shared_work=shared)
        self.assertEqual(result["outcome"], "unavailable")
        self.assertTrue(shared["accounting_unknown"])
        self.assertIsNone(result["native_work"]["decoded_bytes"])
        self.assertEqual(result["native_work"]["known_decoded_bytes"], 71 + recall._bytes(self.catalog))
        with patch("hook_recall._service_client") as next_owner:
            result = recall.collect_reader(self.config, "Investigate the next owner", self.root,
                                           budget=self.budget, shared_work=shared)
        next_owner.assert_not_called()
        self.assertEqual(result["outcome"], "timeout")
        self.assertTrue(result["native_work"]["accounting_unknown"])

    def test_identity_includes_prior_owner_work(self):
        client = self.client()
        with patch("hook_recall.McpClient", return_value=client):
            value = self.identity(spent_native_bytes=123)
        self.assertEqual(value["outcome"], "ok")
        self.assertEqual(value["native_work"], {
            "decoded_bytes": 123 + recall._bytes(self.catalog),
            "read_allowance_bytes": self.budget.native_read_bytes,
            "read_allowance_exhausted": False})
        self.assertEqual(client.calls, [("databases", {})])

    def test_identity_then_association_keeps_one_total_and_legacy_result(self):
        client = self.client()
        with patch("hook_recall.McpClient", return_value=client):
            identity = self.identity(spent_native_bytes=53)
            value = self.association(with_work=True,
                spent_native_bytes=identity["native_work"]["decoded_bytes"])
            legacy = self.association()
        self.assertEqual(legacy, "kept")
        self.assertEqual(value["outcome"], "kept")
        self.assertEqual(value["native_work"]["decoded_bytes"],
            53 + 2 * recall._bytes(self.catalog) + recall._bytes(self.nodes[ID1]))

    def test_exhaustion_refuses_before_config_or_owner_contact(self):
        for spent in (self.budget.native_read_bytes, self.budget.native_read_bytes + 1):
            for operation in ("identity", "association", "overlap"):
                with self.subTest(spent=spent, operation=operation), \
                     patch("hook_recall._project_config") as config, \
                     patch("hook_recall._service_client") as client:
                    if operation == "identity":
                        value = self.identity(spent_native_bytes=spent)
                    elif operation == "association":
                        value = self.association(with_work=True, spent_native_bytes=spent)
                    else:
                        value = recall.collect_overlap(self.config, "cue", self.root,
                            expected_db_id=self.db_id, budget=self.budget, spent_native_bytes=spent)
                    self.assertEqual(value["outcome"], "unavailable")
                    self.assertEqual(value["native_work"]["decoded_bytes"], spent)
                    self.assertTrue(value["native_work"]["read_allowance_exhausted"])
                    config.assert_not_called()
                    client.assert_not_called()

    def test_invalid_prior_charge_does_not_become_a_zero_work_receipt(self):
        for invalid in (True, -1, 1.5, None):
            with self.subTest(invalid=invalid), \
                 patch("hook_recall._service_client") as client:
                for value in (self.identity(spent_native_bytes=invalid),
                              self.association(with_work=True, spent_native_bytes=invalid)):
                    self.assertEqual(value["outcome"], "unavailable")
                    self.assertIsNone(value["native_work"]["decoded_bytes"])
                client.assert_not_called()

    def test_oversized_identity_catalog_is_charged_but_not_admitted(self):
        client = self.client()
        spent = self.budget.native_read_bytes - 1
        with patch("hook_recall.McpClient", return_value=client):
            value = self.identity(spent_native_bytes=spent)
        self.assertEqual(value["outcome"], "unavailable")
        self.assertNotIn("db_id", value)
        self.assertEqual(value["native_work"]["decoded_bytes"], spent + recall._bytes(self.catalog))
        self.assertTrue(value["native_work"]["read_allowance_exhausted"])
        self.assertEqual(client.calls, [("databases", {})])

    def test_association_stops_at_catalog_exhaustion_before_get(self):
        for room in (1, recall._bytes(self.catalog)):
            with self.subTest(room=room):
                client = self.client()
                spent = self.budget.native_read_bytes - room
                with patch("hook_recall.McpClient", return_value=client):
                    value = self.association(with_work=True, spent_native_bytes=spent)
                self.assertEqual(value["outcome"], "unavailable")
                self.assertEqual(value["native_work"]["decoded_bytes"],
                                 spent + recall._bytes(self.catalog))
                self.assertTrue(value["native_work"]["read_allowance_exhausted"])
                self.assertEqual(client.calls, [("databases", {})])

    def test_oversized_get_is_charged_and_cannot_be_kept(self):
        client = self.client()
        spent = self.budget.native_read_bytes - recall._bytes(self.catalog) - 1
        with patch("hook_recall.McpClient", return_value=client):
            value = self.association(with_work=True, spent_native_bytes=spent)
        self.assertEqual(value["outcome"], "unavailable")
        self.assertEqual(value["native_work"]["decoded_bytes"],
            spent + recall._bytes(self.catalog) + recall._bytes(self.nodes[ID1]))
        self.assertTrue(value["native_work"]["read_allowance_exhausted"])
        self.assertEqual([name for name, _ in client.calls], ["databases", "get"])

    def test_stale_association_still_reports_get_work(self):
        self.nodes[ID1]["summary"] = "changed"
        client = self.client()
        with patch("hook_recall.McpClient", return_value=client):
            value = self.association(with_work=True, spent_native_bytes=18)
        self.assertEqual(value["outcome"], "stale")
        self.assertEqual(value["native_work"]["decoded_bytes"],
            18 + recall._bytes(self.catalog) + recall._bytes(self.nodes[ID1]))

    def test_failed_or_undecodable_read_does_not_claim_complete_accounting(self):
        for operation, name in (("identity", "databases"), ("association", "get")):
            for malformed in (False, True):
                with self.subTest(operation=operation, malformed=malformed):
                    client = self.client()
                    original = client.call_tool
                    def read(tool, arguments):
                        if tool == name:
                            client.calls.append((tool, arguments))
                            if malformed:
                                return {"cannot_encode": object()}
                            raise recall.McpError("fixture response unavailable")
                        return original(tool, arguments)
                    client.call_tool = read
                    with patch("hook_recall.McpClient", return_value=client):
                        value = (self.identity(spent_native_bytes=61) if operation == "identity"
                                 else self.association(with_work=True, spent_native_bytes=61))
                    self.assertEqual(value["outcome"], "unavailable")
                    self.assertIsNone(value["native_work"]["decoded_bytes"])
                    expected = 61 + (recall._bytes(self.catalog) if operation == "association" else 0)
                    self.assertEqual(value["native_work"]["known_decoded_bytes"], expected)
                    self.assertEqual([tool for tool, _ in client.calls],
                                     ["databases"] if operation == "identity" else ["databases", "get"])

    def test_association_catalog_latency_does_not_restart_the_deadline(self):
        client = self.client()
        original = client.call_tool
        clock = [100.0]
        def read(name, arguments):
            value = original(name, arguments)
            clock[0] += 1.1
            return value
        client.call_tool = read
        with patch("hook_recall.time.monotonic", side_effect=lambda: clock[0]), \
             patch("hook_recall.McpClient", return_value=client):
            value = self.association(with_work=True, spent_native_bytes=9)
        self.assertEqual(value["outcome"], "unavailable")
        self.assertEqual(value["native_work"]["decoded_bytes"], 9 + recall._bytes(self.catalog))
        self.assertEqual(client.calls, [("databases", {})])

    def test_association_cleanup_is_inside_the_whole_call_deadline(self):
        clock = [100.0]
        class SlowCleanup(FakeClient):
            def __exit__(self, *_):
                clock[0] += 1.1
                return False
        client = SlowCleanup(self.catalog, self.context, self.nodes)
        with patch("hook_recall.time.monotonic", side_effect=lambda: clock[0]), \
             patch("hook_recall.McpClient", return_value=client):
            value = self.association(with_work=True)
        self.assertEqual(value["outcome"], "unavailable")
        self.assertEqual(value["native_work"]["decoded_bytes"],
                         recall._bytes(self.catalog) + recall._bytes(self.nodes[ID1]))


if __name__ == "__main__":
    unittest.main()
