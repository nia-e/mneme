"""Synthetic, disposable diagnostics only: no owners, hooks, network or live state."""
from __future__ import annotations

import copy
import json
import os
from pathlib import Path
import stat
import tempfile
import unittest
from unittest.mock import patch

import host_files
import session_activity as activity

OWNER = {"name": "project", "id": "owner-example", "db": "project"}


class ActivityWriterTests(unittest.TestCase):
    def setUp(self):
        temporary = tempfile.TemporaryDirectory()
        self.addCleanup(temporary.cleanup)
        self.temp = Path(temporary.name).resolve()
        self.workspace = self.temp / "workspace"
        self.workspace.mkdir()
        self.root = self.temp / "activity"
        host_files.ensure_private_directory(self.root)
        self.manifest = self.temp / "activity.json"
        self.config = {"schema": activity.CONFIG_SCHEMA, "root": str(self.root),
                       "workspaces": [{"path": str(self.workspace), "source": "codex"}], "device": None}
        self.write(self.manifest, self.config)
        self.environment = patch.dict(os.environ, {"MNEME_ACTIVITY_CONFIG": str(self.manifest)})
        self.environment.start()
        self.addCleanup(self.environment.stop)
        # Deterministic occurrence/retention time; diagnostics only, never models.
        self.clock = patch.object(activity.time, "time", return_value=1_800_000_000)
        self.clock.start()
        self.addCleanup(self.clock.stop)

    def write(self, path, data):
        host_files.atomic_write(path, activity._encode(data))

    def event(self, session="session-1", turn="turn-1", cwd=None):
        return {"session_id": session, "turn_id": turn, "cwd": str(cwd or self.workspace),
                "prompt": "SECRET prompt", "query": "SECRET query", "transcript": "SECRET transcript"}

    def emit(self, session="session-1", **kwargs):
        return activity.emit(self.event(session), "lifecycle", "startup", **kwargs)

    def path(self, session="session-1"):
        return self.root / activity.filename(session)

    def record(self, session="session-1"):
        return json.loads(self.path(session).read_bytes())

    def quota(self):
        return json.loads((self.root / activity.QUOTA).read_bytes())

    def inventory(self):
        return {path.name: path.read_bytes() for path in self.root.iterdir() if path.is_file()}

    def test_shared_contract_fixture(self):
        fixture = Path(__file__).parent / "fixtures" / "session-activity.json"
        data = json.loads(fixture.read_bytes())
        activity._validate_record(data)
        self.assertLessEqual(len(activity._encode(data)), activity.RECORD_BYTES)
        for item in data["events"]:
            kwargs = {key: value for key, value in item.items() if key not in ("kind", "outcome")}
            self.assertTrue(activity.emit(self.event(), item["kind"], item["outcome"], **kwargs))
        emitted = self.record()
        self.assertEqual(emitted["events"], data["events"])
        self.assertEqual(emitted["first_observed"], data["first_observed"])
        self.assertEqual(emitted["last_observed"], data["last_observed"])

    def test_private_metadata_only_and_exact_origin(self):
        self.assertTrue(activity.emit(self.event(), "recall_selection", "selected", owner=OWNER,
            nodes=["node-1"], count=1, origin={"generation": "ab123", "context_generation": 4,
                                           "attempt": "attempt-2"}, selection_digest="a" * 64))
        record = self.record()
        self.assertEqual(record["source"], "codex")
        self.assertIsNone(record["device"])
        self.assertEqual(record["workspace"], str(self.workspace))
        self.assertEqual(record["events"][0]["origin"], {"turn": "turn-1", "generation": "ab123",
                         "context_generation": 4, "attempt": "attempt-2"})
        self.assertNotIn(b"SECRET", self.path().read_bytes())
        for path in self.root.iterdir():
            host_files.check_private(path)
        self.assertEqual(len(self.quota()["reservations"]), 1)
        self.assertFalse((self.root / activity.STAGE).exists())

    def test_metadata_containers_detached_before_filesystem_work(self):
        owner, nodes = dict(OWNER), ["node-1"]
        binding = activity._binding
        def mutate_after_projection(event):
            owner["body"] = "SECRET owner body"
            nodes.append("SECRET node body")
            return binding(event)
        with patch.object(activity, "_binding", side_effect=mutate_after_projection):
            self.assertTrue(activity.emit(self.event(), "recall_selection", "selected", owner=owner, nodes=nodes))
        self.assertEqual(self.record()["events"][0]["owner"], OWNER)
        self.assertEqual(self.record()["events"][0]["nodes"], ["node-1"])
        self.assertNotIn(b"SECRET", self.path().read_bytes())

    def test_dedup_and_real_attempts_remain_distinct(self):
        self.assertTrue(self.emit(at=100))
        before = self.inventory()
        self.assertTrue(self.emit(at=105))
        self.assertEqual(self.inventory(), before)
        self.assertTrue(activity.emit(self.event(turn="turn-2"), "lifecycle", "startup", at=106))
        self.assertEqual(len(self.record()["events"]), 2)
        self.assertFalse(self.emit(key=self.record()["events"][0]["key"], reason="conflicting"))

    def test_late_receipt_keeps_append_order_without_rewriting_occurrence(self):
        self.assertTrue(activity.emit(self.event(), "lifecycle", "end", at=200, key="end"))
        self.assertTrue(activity.emit(self.event(), "recall_selection", "selected", at=100, key="late"))
        record = self.record()
        self.assertEqual([item["key"] for item in record["events"]], ["end", "late"])
        self.assertEqual((record["first_observed"], record["last_observed"]), (100, 200))
        self.assertEqual(record["events"][0]["outcome"], "end")

    def test_missing_malformed_and_unbound_config_do_not_touch_root(self):
        for config in (None, {}, {**self.config, "schema": "future"},
                       {**self.config, "workspaces": []},
                       {**self.config, "workspaces": self.config["workspaces"] * 2},
                       {**self.config, "secret": "do not retain"}):
            with self.subTest(config=config):
                if config is None:
                    self.manifest.unlink()
                else:
                    self.write(self.manifest, config)
                self.assertFalse(self.emit())
                self.assertEqual(self.inventory(), {})
        self.write(self.manifest, self.config)
        self.assertFalse(activity.emit(self.event(cwd=self.temp), "lifecycle", "startup"))
        self.assertEqual(self.inventory(), {})

    def test_default_manifest_uses_platform_config_directory(self):
        with patch.dict(os.environ, {}, clear=True), \
                patch.object(activity.host_platform, "default_config_dir", return_value=self.temp) as default:
            self.assertTrue(self.emit())
            default.assert_called_once_with()

    def test_no_implicit_root_creation(self):
        self.root.rmdir()
        self.assertFalse(self.emit())
        self.assertFalse(self.root.exists())

    def test_nearest_workspace_nested_boundary_and_rebinding(self):
        nested = self.workspace / "nested"
        nested.mkdir()
        (nested / ".git").write_text("gitdir: fixture-only")
        child = nested / "src"
        child.mkdir()
        self.assertFalse(activity.emit(self.event(cwd=child), "lifecycle", "startup"))
        self.assertEqual(self.inventory(), {})
        self.config["workspaces"].append({"path": str(nested), "source": "codex"})
        self.write(self.manifest, self.config)
        self.assertTrue(activity.emit(self.event(cwd=child), "lifecycle", "startup"))
        self.assertEqual(self.record()["workspace"], str(nested))
        before = self.inventory()
        self.assertFalse(self.emit())
        self.assertEqual(self.inventory(), before)

    def test_unrelated_missing_workspace_does_not_disable_current_binding(self):
        missing = self.temp / "deleted-project"
        self.config["workspaces"].append({"path": str(missing), "source": "codex"})
        self.write(self.manifest, self.config)
        manifest_before = self.manifest.read_bytes()
        self.assertTrue(self.emit())
        self.assertEqual(self.record()["workspace"], str(self.workspace))
        self.assertEqual(self.manifest.read_bytes(), manifest_before)
        self.assertFalse(missing.exists())

    def test_unrelated_binding_metadata_remains_strict(self):
        for path in ("relative", str(self.temp) + "/missing/../other",
                     str(self.temp) + "//missing", str(self.temp) + "/missing/."):
            with self.subTest(path=path):
                config = copy.deepcopy(self.config)
                config["workspaces"].append({"path": path, "source": "codex"})
                self.write(self.manifest, config)
                self.assertFalse(self.emit())
                self.assertEqual(self.inventory(), {})

    def test_selected_binding_identity_change_refuses_without_parent_fallback(self):
        self.config["workspaces"].insert(0, {"path": str(self.temp), "source": "codex"})
        self.write(self.manifest, self.config)
        child = self.workspace / "src"
        child.mkdir()
        resolve = Path.resolve
        def changed_identity(path, **kwargs):
            return self.temp if path == self.workspace else resolve(path, **kwargs)
        with patch.object(Path, "resolve", changed_identity):
            self.assertFalse(activity.emit(self.event(cwd=child), "lifecycle", "startup"))
        self.assertEqual(self.inventory(), {})

    def test_defer_has_no_filesystem_work_until_outer_exit(self):
        with activity.defer() as batch:
            with patch.object(activity, "_binding", side_effect=AssertionError("inside functional budget")):
                self.assertFalse(self.emit(at=123))
                with activity.defer() as nested:
                    self.assertIs(nested, batch)
                    self.assertFalse(activity.emit(self.event(turn="turn-2"), "core", "prepared", count=0, at=124))
                self.assertFalse(batch.recorded)
                self.assertFalse(batch.recorded_lifecycle)
                self.assertEqual(self.inventory(), {})
        self.assertTrue(batch.recorded)
        self.assertTrue(batch.recorded_lifecycle)
        self.assertEqual([item["at"] for item in self.record()["events"]], [123, 124])
        self.assertEqual(batch._pending, [])

    def test_defer_keeps_only_detached_allowlisted_metadata(self):
        event = self.event()
        owner, nodes = dict(OWNER), ["node-1"]
        with activity.defer() as batch:
            self.assertFalse(activity.emit(event, "recall_selection", "selected", owner=owner, nodes=nodes))
            self.assertNotIn(b"SECRET", activity._encode(batch._pending))
            event["session_id"] = "changed"
            owner["body"] = "SECRET added body"
            nodes.append("SECRET added node")
        self.assertTrue(batch.recorded)
        self.assertFalse(batch.recorded_lifecycle)
        self.assertFalse(self.path("changed").exists())
        self.assertNotIn(b"SECRET", self.path().read_bytes())
        self.assertEqual(self.record()["events"][0]["nodes"], ["node-1"])

    def test_defer_rejects_raw_turn_even_when_origin_overrides_it(self):
        for turn in ({"body": "SECRET"}, ["SECRET"], "x" * 257):
            with self.subTest(turn=type(turn).__name__), activity.defer() as batch:
                with patch.object(activity, "_encode", side_effect=AssertionError("must reject before encoding")):
                    self.assertFalse(activity.emit(self.event(turn=turn), "core", "prepared",
                                                   origin={"turn": "safe"}))
                self.assertEqual(batch._pending, [])
        self.assertEqual(self.inventory(), {})

    def test_defer_queue_and_drain_deadline_are_bounded(self):
        with patch.object(activity, "DEFER_BYTES", 1), activity.defer() as batch:
            self.assertFalse(self.emit())
            self.assertEqual(batch._pending, [])
        self.assertFalse(batch.recorded)
        self.assertEqual(self.inventory(), {})
        with patch.object(activity, "DEFER_SECONDS", 0), activity.defer() as batch:
            self.assertFalse(self.emit())
        self.assertFalse(batch.recorded)
        self.assertEqual(self.inventory(), {})
        real_append = activity._append
        def use_drain_budget(*args):
            result = real_append(*args)
            # A deterministic slow filesystem stand-in; the next receipt must
            # not receive a fresh quarter-second budget.
            activity.time.monotonic = lambda: float("inf")
            return result
        with patch.object(activity.time, "monotonic", wraps=activity.time.monotonic), \
                patch.object(activity, "_append", side_effect=use_drain_budget) as append:
            with activity.defer() as batch:
                self.assertFalse(self.emit())
                self.assertFalse(activity.emit(self.event(turn="turn-2"), "core", "prepared"))
            self.assertTrue(batch.recorded_lifecycle)
            self.assertEqual(append.call_count, 1)
        self.assertEqual(len(self.record()["events"]), 1)

    def test_host_admission_preserves_recording_origin_without_guessing_selector(self):
        selector = {"turn": "turn-1", "generation": "reader-generation", "context_generation": 7,
                    "attempt": "selector-attempt"}
        recording = {"turn": "turn-1", "generation": None, "context_generation": None,
                     "attempt": "recording:" + "a" * 64}
        self.assertTrue(activity.emit(self.event(), "recall_selection", "selected", origin=selector,
                                      owner=OWNER, nodes=["node-1"], count=1, at=100))
        with activity.defer() as batch:
            self.assertFalse(activity.emit(self.event(), "host_admission", "admitted_retained",
                reason="selector_origin_unavailable", origin=recording, owner=OWNER,
                nodes=["node-1"], count=1, at=110))
        self.assertTrue(batch.recorded)
        observed = self.record()["events"][-1]
        self.assertEqual(observed["origin"], recording)
        self.assertEqual(observed["reason"], "selector_origin_unavailable")
        self.assertIsNone(observed["selection_digest"])
        self.assertEqual(observed["nodes"], ["node-1"])

    def test_defer_failures_do_not_replace_real_outcome_or_exception(self):
        with patch.object(activity, "_append", side_effect=RuntimeError("SECRET provider output")):
            with activity.defer() as batch:
                self.assertFalse(self.emit())
                result = "normal output"
            self.assertEqual(result, "normal output")
            self.assertFalse(batch.recorded)
            with self.assertRaisesRegex(ValueError, "real operation"):
                with activity.defer():
                    self.emit()
                    raise ValueError("real operation")
        self.assertEqual(self.inventory(), {})
        self.assertIsNone(activity._DEFERRED.get())

    def test_mneme_project_boundary(self):
        nested = self.workspace / "nested"
        nested.mkdir()
        (nested / ".mneme").mkdir()
        self.assertFalse(activity.emit(self.event(cwd=nested), "lifecycle", "startup"))
        self.assertEqual(self.inventory(), {})

    def test_invalid_metadata_never_logs_exception_or_extra_fields(self):
        cases = [{"reason": "secret query = password"}, {"origin": {"body": "SECRET"}},
                 {"owner": {**OWNER, "endpoint": "SECRET"}}, {"owner": {**OWNER, "db": "/private/path"}},
                 {"nodes": ["node"]}, {"owner": OWNER, "nodes": ["body with spaces"]},
                 {"count": True}, {"at": -1}, {"origin": {"generation": []}},
                 {"origin": {"context_generation": True}}, {"selection_digest": "not-a-digest"},
                 {"omitted": 2**64}, {"key": "newline\nprivate"}]
        for kwargs in cases:
            with self.subTest(kwargs=kwargs):
                self.assertFalse(self.emit(**kwargs))
                self.assertEqual(self.inventory(), {})
        with patch.object(activity, "_binding", side_effect=RuntimeError("SECRET provider response")):
            self.assertFalse(self.emit())
        self.assertEqual(self.inventory(), {})

    def test_contention_is_nonblocking_and_preserves_payload(self):
        self.assertTrue(self.emit())
        before = self.inventory()
        fd = host_files.open_regular(self.root / activity.LOCK, os.O_RDWR)
        try:
            host_files.lock_file(fd, blocking=False)
            self.assertFalse(activity.emit(self.event(turn="turn-2"), "lifecycle", "resume"))
        finally:
            os.close(fd)
        self.assertEqual(self.inventory(), before)

    def test_routine_append_does_not_enumerate_root(self):
        self.assertTrue(self.emit())
        with patch.object(activity.os, "scandir", side_effect=AssertionError("routine root scan")):
            self.assertTrue(activity.emit(self.event(turn="turn-2"), "lifecycle", "resume"))
        self.assertEqual(len(self.record()["events"]), 2)

    def test_event_eviction_is_byte_bounded_and_counted(self):
        nodes = [f"node-{index}-" + "x" * 100 for index in range(200)]
        for index in range(5):
            self.assertTrue(activity.emit(self.event(turn=f"turn-{index}"), "recall_selection", "selected",
                                          owner=OWNER, nodes=nodes, count=len(nodes), at=index + 100))
        record = self.record()
        self.assertLessEqual(self.path().stat().st_size, activity.RECORD_BYTES)
        self.assertGreater(record["evicted_events"], 0)
        self.assertEqual(record["events"][-1]["origin"]["turn"], "turn-4")
        self.assertEqual(record["first_observed"], 100)
        self.assertEqual(record["last_observed"], 104)

    def test_node_overflow_is_explicit_not_content_leak(self):
        with patch.object(activity, "RECORD_BYTES", 1300):
            self.assertTrue(activity.emit(self.event(), "recall_selection", "selected", owner=OWNER,
                                          nodes=["node-" + str(i) + "x" * 90 for i in range(20)], count=20))
            record = self.record()
            self.assertLessEqual(self.path().stat().st_size, 1300)
            observation = record["events"][0]
            self.assertEqual(len(observation["nodes"]) + observation["omitted"], 20)
            before = self.path().read_bytes()
            self.assertTrue(activity.emit(self.event(), "recall_selection", "selected", owner=OWNER,
                                          nodes=["node-" + str(i) + "x" * 90 for i in range(20)], count=20))
            self.assertEqual(self.path().read_bytes(), before)

    def test_quota_cap_reserves_full_allowance_and_retires_oldest(self):
        with patch.object(activity, "PAYLOAD_BYTES", 2 * activity.RECORD_BYTES):
            self.assertTrue(self.emit("oldest", at=1_800_000_000))
            self.assertTrue(self.emit("newer", at=1_800_000_010))
            self.assertTrue(self.emit("third", at=1_800_000_020))
            self.assertFalse(self.path("oldest").exists())
            self.assertTrue(self.path("newer").exists())
            self.assertTrue(self.path("third").exists())
            self.assertEqual(len(self.quota()["reservations"]), 2)

    def test_retention_only_deletes_recognized_expired_records(self):
        self.assertTrue(self.emit("expired", at=1_800_000_000 - activity.RETENTION_SECONDS - 1))
        self.assertTrue(self.emit("current"))
        self.assertFalse(self.path("expired").exists())
        control = self.root / "control.json"
        control.write_bytes(b"untouchable")
        before = self.inventory()
        self.assertFalse(self.emit("another"))
        self.assertEqual(self.inventory(), before)

    def test_missing_quota_recovers_only_recognized_records(self):
        self.assertTrue(self.emit())
        (self.root / activity.QUOTA).unlink()
        self.assertTrue(activity.emit(self.event(turn="turn-2"), "lifecycle", "resume"))
        self.assertEqual(len(self.quota()["reservations"]), 1)
        (self.root / activity.QUOTA).unlink()
        self.write(self.root / "unknown.json", {"future": True})
        before = self.inventory()
        self.assertFalse(self.emit())
        self.assertEqual(self.inventory(), before)

    def test_reserved_but_missing_payload_recovers_only_same_workspace(self):
        self.assertTrue(self.emit())
        self.path().unlink()
        other = self.workspace / "other"
        other.mkdir()
        self.config["workspaces"].append({"path": str(other), "source": "codex"})
        self.write(self.manifest, self.config)
        self.assertFalse(activity.emit(self.event(cwd=other), "lifecycle", "resume"))
        self.assertTrue(self.emit())
        self.assertEqual(len(self.quota()["reservations"]), 1)

    def test_quota_publication_precedes_record_and_crash_is_recoverable(self):
        publish = activity._publish
        def fail_record(root, name, value, limit):
            if name == activity.filename("session-1"):
                raise OSError("simulated crash")
            return publish(root, name, value, limit)
        with patch.object(activity, "_publish", side_effect=fail_record):
            self.assertFalse(self.emit())
        self.assertFalse(self.path().exists())
        self.assertIn(self.path().name, self.quota()["reservations"])
        self.assertTrue(self.emit())
        self.assertEqual(len(self.quota()["reservations"]), 1)

    def test_known_crash_stage_discarded_unknown_stage_preserved(self):
        self.assertTrue(self.emit())
        self.write(self.root / activity.STAGE, self.record())
        self.assertTrue(activity.emit(self.event(turn="turn-2"), "lifecycle", "resume"))
        self.assertFalse((self.root / activity.STAGE).exists())
        host_files.atomic_write(self.root / activity.STAGE, b'{"schema":')
        before = self.inventory()
        self.assertFalse(self.emit())
        self.assertEqual(self.inventory(), before)

    def test_unsafe_or_unknown_retained_state_refuses_without_repair(self):
        self.assertTrue(self.emit())
        valid = self.record()
        variants = [{**valid, "schema": "future"}, {**valid, "body": "SECRET"},
                    {**valid, "session_id": "wrong-id"}]
        for variant in variants:
            self.write(self.path(), variant)
            before = self.inventory()
            self.assertFalse(self.emit())
            self.assertEqual(self.inventory(), before)
        self.write(self.path(), valid)
        self.write(self.root / activity.QUOTA, {"schema": "future", "reservations": {}})
        before = self.inventory()
        self.assertFalse(self.emit())
        self.assertEqual(self.inventory(), before)

    def test_oversize_duplicate_key_deep_json_and_fifo_manifest(self):
        for data in (b" " * (activity.RECORD_BYTES + 1),
                     b'{"schema":"a","schema":"b"}', b"[" * 1000 + b"]" * 1000,
                     b'{"integer":' + b"9" * 60000 + b'}', b'{"float":1e99999}',
                     json.dumps(self.config).encode("utf-16"), b"\xef\xbb\xbf" + activity._encode(self.config)):
            host_files.atomic_write(self.manifest, data)
            self.assertFalse(self.emit())
            self.assertEqual(self.inventory(), {})
        if hasattr(os, "mkfifo"):
            self.manifest.unlink()
            os.mkfifo(self.manifest)
            self.assertFalse(self.emit())
            self.assertEqual(self.inventory(), {})

    @unittest.skipIf(os.name == "nt", "Unix symlink fixture")
    def test_symlink_leaf_parent_and_hardlink_rejected(self):
        self.assertTrue(self.emit())
        original = self.path().read_bytes()
        outside = self.temp / "outside"
        host_files.atomic_write(outside, original)
        self.path().unlink()
        self.path().symlink_to(outside)
        self.assertFalse(self.emit())
        self.assertEqual(outside.read_bytes(), original)
        self.path().unlink()
        os.link(outside, self.path())
        self.assertFalse(self.emit())
        self.assertEqual(outside.read_bytes(), original)
        self.path().unlink()
        alias = self.temp / "alias"
        alias.symlink_to(self.root, target_is_directory=True)
        self.config["root"] = str(alias)
        self.write(self.manifest, self.config)
        self.assertFalse(self.emit())

    @unittest.skipIf(os.name == "nt", "Unix permission fixture")
    def test_nonprivate_root_manifest_and_record_refused_without_chmod(self):
        self.manifest.chmod(0o644)
        self.assertFalse(self.emit())
        self.assertEqual(stat.S_IMODE(self.manifest.stat().st_mode), 0o644)
        self.manifest.chmod(0o600)
        self.root.chmod(0o755)
        self.assertFalse(self.emit())
        self.assertEqual(stat.S_IMODE(self.root.stat().st_mode), 0o755)
        self.root.chmod(0o700)
        self.assertTrue(self.emit())
        self.path().chmod(0o644)
        before = self.path().read_bytes()
        self.assertFalse(self.emit())
        self.assertEqual(self.path().read_bytes(), before)
        self.assertEqual(stat.S_IMODE(self.path().stat().st_mode), 0o644)

    def test_admission_work_and_quota_metadata_are_bounded(self):
        with patch.object(activity, "ADMISSION_ENTRIES", 0):
            self.assertFalse(self.emit())
        self.assertFalse(self.path().exists())
        with patch.object(activity, "QUOTA_BYTES", 10):
            self.assertFalse(self.emit())
        self.assertFalse(self.path().exists())
        self.assertTrue(self.emit())
        before = self.inventory()
        with patch.object(activity, "ADMISSION_BYTES", 1):
            # The established reservation ledger proves spare capacity without
            # pretending a byte-limited retention pass inspected everything.
            self.assertTrue(self.emit("new-session"))
        self.assertEqual(self.path().read_bytes(), before[self.path().name])
        before = self.inventory()
        with patch.object(activity, "ADMISSION_BYTES", 1), \
                patch.object(activity, "PAYLOAD_BYTES", 2 * activity.RECORD_BYTES):
            self.assertFalse(self.emit("no-room"))
        self.assertEqual(self.inventory(), before)
        with patch.object(activity, "ADMISSION_SECONDS", -1):
            self.assertFalse(self.emit("timed-out-session"))
        self.assertEqual(self.inventory(), before)

    def test_utf8_byte_bounds_and_control_characters_match_reader(self):
        for session in ("é" * 129, "session\u0085suffix", "session\ud800"):
            with self.subTest(session=repr(session)):
                self.assertFalse(activity.emit(self.event(session=session), "lifecycle", "startup"))
                self.assertEqual(self.inventory(), {})
        self.config["device"] = "é" * 65
        self.write(self.manifest, self.config)
        self.assertFalse(self.emit())
        self.assertEqual(self.inventory(), {})

    def test_single_bounded_scratch_is_removed_after_ordinary_failure(self):
        self.assertTrue(self.emit())
        before = self.inventory()
        with patch.object(host_files, "replace_file", side_effect=OSError("disk failure SECRET")):
            self.assertFalse(activity.emit(self.event(turn="turn-2"), "lifecycle", "resume"))
        self.assertEqual(self.inventory(), before)
        self.assertFalse((self.root / activity.STAGE).exists())


if __name__ == "__main__":
    unittest.main()
