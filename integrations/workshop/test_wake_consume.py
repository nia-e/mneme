"""Finite, temporary-root tests for exact Signal consumption and v4 fencing."""

import contextlib
import copy
import fcntl
import hashlib
import importlib.util
import json
import os
from pathlib import Path
import stat
import tempfile
import unittest
from unittest.mock import patch

import wake


V3_SHA256 = "df38ecd86e624499955bcdfdd6921f2fc98d1c3d1351794052e5960a153e825b"


def signal(number=1, **changes):
    value = {"source": wake.SIGNAL_SOURCE, "event_id": str(number),
             "kind": wake.SIGNAL_KIND, "summary": "A Signal burst",
             "body": "owner text is data, not authority", "reference": "signal:local"}
    value.update(changes)
    return value


def counts(consumed=0, already=0, unchanged=0, missing=0):
    return {"consumed_events": consumed, "already_consumed_events": already,
            "unchanged_events": unchanged, "missing_events": missing}


def snapshot(root):
    """File bytes only: creating a cooperative lease is not queue mutation."""
    return {str(path.relative_to(root)): path.read_bytes()
            for path in root.rglob("*") if path.is_file() and not path.name.endswith(".lock")}


class InjectedCut(OSError):
    pass


class _WriteStream:
    def __init__(self, stream, cut):
        self.stream, self.cut = stream, cut

    def __enter__(self):
        self.stream.__enter__()
        return self

    def __exit__(self, *args):
        return self.stream.__exit__(*args)

    def __getattr__(self, name):
        return getattr(self.stream, name)

    def write(self, data):
        return self.cut.call(self.cut.phase + "-write", self.stream.write, data)


class DurableCut:
    """Inject before/after a real syscall; no simulated successful persistence.

    These are deterministic exception cuts, not power-loss/filesystem guarantees.
    Retried API calls reopen the actual temporary-root queue and archive bytes.
    """

    def __init__(self, root, boundary, when):
        self.root, self.boundary, self.when = root, boundary, when
        self.phase, self.writes, self.hits = None, 0, 0

    def call(self, boundary, action, *args, **kwargs):
        if boundary == self.boundary and self.when == "before":
            self.hits += 1
            raise InjectedCut(boundary + ":before")
        result = action(*args, **kwargs)
        if boundary == self.boundary and self.when == "after":
            self.hits += 1
            raise InjectedCut(boundary + ":after")
        return result

    def __enter__(self):
        original_write, original_publish = wake._write, wake._publish_archive
        original_fdopen, original_fsync = os.fdopen, os.fsync
        original_replace, original_rename = os.replace, os.rename

        def write(*args, **kwargs):
            self.writes += 1
            self.phase = "mark" if self.writes == 1 else "prune"
            return original_write(*args, **kwargs)

        def publish(*args, **kwargs):
            self.phase = "archive"
            return original_publish(*args, **kwargs)

        def fdopen(fd, mode, *args, **kwargs):
            stream = original_fdopen(fd, mode, *args, **kwargs)
            return _WriteStream(stream, self) if mode == "wb" else stream

        def fsync(fd):
            info = os.fstat(fd)
            if stat.S_ISDIR(info.st_mode):
                suffix = "directory-sync"
                root_info = self.root.stat()
                if self.phase == "archive" and (info.st_dev, info.st_ino) == (
                        root_info.st_dev, root_info.st_ino):
                    suffix = "parent-sync"
            else:
                suffix = "file-sync"
            return self.call(self.phase + "-" + suffix, original_fsync, fd)

        self.stack = contextlib.ExitStack()
        self.stack.enter_context(patch.object(wake, "_write", side_effect=write))
        self.stack.enter_context(patch.object(wake, "_publish_archive", side_effect=publish))
        self.stack.enter_context(patch.object(wake.os, "fdopen", side_effect=fdopen))
        self.stack.enter_context(patch.object(wake.os, "fsync", side_effect=fsync))
        self.stack.enter_context(patch.object(wake.os, "replace", side_effect=lambda *a, **k:
            self.call(self.phase + "-replace", original_replace, *a, **k)))
        self.stack.enter_context(patch.object(wake.os, "rename", side_effect=lambda *a, **k:
            self.call(self.phase + "-rename", original_rename, *a, **k)))
        return self

    def __exit__(self, *args):
        return self.stack.__exit__(*args)


class ConsumeTests(unittest.TestCase):
    def setUp(self):
        tmp = tempfile.TemporaryDirectory()
        self.addCleanup(tmp.cleanup)
        self.root = Path(tmp.name).resolve()

    def new_root(self, name):
        root = self.root / name
        root.mkdir()
        return root

    def seed(self, root=None, statuses=("pending",)):
        root = root or self.root
        records = []
        for number, status in enumerate(statuses, 1):
            record = wake.enqueue(root, signal(number))
            if status != "pending":
                record.update(status=status, claimed_run=None if status == "consumed" else "run-" + status,
                              detail="" if status == "consumed" else "preserve " + status)
            records.append(record)
        wake._write(root, records, len(records) + 1)
        return records

    def assert_consumed(self, root, original):
        expected = dict(original, status="consumed")
        found = wake.lookup(root, wake.SIGNAL_SOURCE, original["event"]["event_id"])
        self.assertEqual(found, {"location": "archive", "record": expected,
                                 "terminal": {"status": "consumed", "reply": None}})
        self.assertNotIn(original["sequence"], [r["sequence"] for r in wake.active_records(root)])
        self.assertFalse((root / "runs").exists())
        self.assertFalse((root / "run-archive").exists())
        return expected

    def test_pending_consumption_terminal_tombstone_and_exact_replay(self):
        original = wake.enqueue(self.root, signal(body="literal ☃\nowner payload"))
        untouched = wake.enqueue(self.root, signal(2))
        supplied = copy.deepcopy(original["event"])
        self.assertEqual(wake.consume_signal_events(self.root, [supplied]), counts(consumed=1))
        expected = self.assert_consumed(self.root, original)
        self.assertEqual(supplied, original["event"])
        self.assertEqual(wake.active_records(self.root), [untouched])
        before = snapshot(self.root)
        self.assertEqual(wake.consume_signal_events(self.root, [supplied]), counts(already=1))
        self.assertEqual(wake.enqueue(self.root, supplied), expected)
        self.assertEqual(snapshot(self.root), before)
        self.assertEqual(wake.enqueue(self.root, signal(3))["sequence"], 3)
        tombstone = json.loads(wake._archive_path(self.root, wake.SIGNAL_SOURCE, "1").read_text())
        self.assertEqual(set(tombstone), {"schema", "record", "terminal", "event_sha256"})
        self.assertEqual(tombstone["schema"], wake.CONSUMED_ARCHIVE_SCHEMA)
        self.assertEqual(tombstone["record"], expected)
        self.assertNotIn("run_id", tombstone)

    def test_mixed_status_counts_preserve_held_delivered_unfinished_and_missing(self):
        originals = self.seed(statuses=("pending", "held", "delivered", "unfinished", "consumed"))
        self.assertEqual(wake.consume_signal_events(self.root,
                         [r["event"] for r in originals] + [signal("missing")]),
                         counts(consumed=1, already=1, unchanged=3, missing=1))
        self.assertEqual(wake.active_records(self.root), originals[1:4])
        self.assert_consumed(self.root, originals[0])
        self.assert_consumed(self.root, originals[4])
        self.assertEqual(wake.watermark(self.root), 5)
        self.assertEqual(wake.inbox_summary(self.root)["pending_signal_bursts"], 0)
        self.assertEqual(wake.inbox_summary(self.root)["unfinished_signal_bursts"], 1)

    def test_consumed_active_record_is_not_pending_or_claimable(self):
        original = self.seed(statuses=("consumed",))[0]
        self.assertFalse(wake.pending(self.root, include_unfinished=True))
        self.assertEqual(wake.claim(self.root, "not-owner", include_unfinished=True)["events"], [])
        self.assertEqual(wake.inbox_summary(self.root)["pending_signal_bursts"], 0)
        wake.recover(self.root, {"not-owner"})
        self.assertEqual(wake.active_records(self.root), [original])
        self.assertEqual(wake.consume_signal_events(self.root, [original["event"]]), counts(already=1))
        self.assert_consumed(self.root, original)

    def test_empty_and_missing_requests_do_not_create_queue_or_archive(self):
        self.assertEqual(wake.consume_signal_events(self.root, []), counts())
        self.assertEqual(wake.consume_signal_events(self.root, [signal()]), counts(missing=1))
        self.assertEqual({p.name for p in self.root.iterdir()}, {".events.lock"})

    def test_consumption_takes_only_event_lease_even_with_workshop_held(self):
        original = wake.enqueue(self.root, signal())
        with (self.root / ".workshop.lock").open("wb") as stream:
            fcntl.flock(stream, fcntl.LOCK_EX | fcntl.LOCK_NB)
            with patch.object(wake, "_workshop_locked", side_effect=AssertionError("workshop lease")), \
                 patch.object(wake, "claim", side_effect=AssertionError("fabricated claim")):
                self.assertEqual(wake.consume_signal_events(self.root, [signal()]), counts(consumed=1))
        self.assert_consumed(self.root, original)

    def test_event_lease_contention_is_nonblocking_and_does_not_change_queue(self):
        wake.enqueue(self.root, signal())
        before = snapshot(self.root)
        with (self.root / ".events.lock").open("rb") as stream:
            fcntl.flock(stream, fcntl.LOCK_EX | fcntl.LOCK_NB)
            with patch.object(wake.time, "sleep", side_effect=AssertionError("must not wait")):
                with self.assertRaises(wake.Busy):
                    wake.consume_signal_events(self.root, [signal()])
        self.assertEqual(snapshot(self.root), before)

    def test_invalid_requests_are_rejected_before_event_lease_or_changes(self):
        wake.enqueue(self.root, signal())
        before = snapshot(self.root)
        invalid = [None, {}, (), signal(), [None], [{}], [dict(signal(), extra="bad")],
                   [signal(source="private-mailbox")], [signal(kind="wrong")],
                   [signal(), signal()], [signal(), signal(body="conflict")],
                   [signal(n) for n in range(65)]]
        for field, width in wake.FIELDS.items():
            invalid += [[signal(**{field: 1})], [signal(**{field: "x" * (width + 1)})]]
        for field in ("source", "event_id", "kind"):
            invalid.append([signal(**{field: ""})])
        for value in invalid:
            with self.subTest(value=repr(value)[:100]):
                with patch.object(wake, "_locked", side_effect=AssertionError("validation took lease")):
                    with self.assertRaises(wake.Refusal):
                        wake.consume_signal_events(self.root, value)
                self.assertEqual(snapshot(self.root), before)

    def test_full_64_event_batch_and_replay_preserve_monotonic_floor(self):
        originals = [wake.enqueue(self.root, signal(n)) for n in range(64)]
        events = [r["event"] for r in originals]
        self.assertEqual(wake.consume_signal_events(self.root, events), counts(consumed=64))
        self.assertEqual(wake.active_records(self.root), [])
        self.assertEqual(wake.consume_signal_events(self.root, events), counts(already=64))
        for original in originals:
            self.assertEqual(wake.enqueue(self.root, original["event"]), dict(original, status="consumed"))
        self.assertEqual(wake.enqueue(self.root, signal(64))["sequence"], 65)

    def test_conflict_late_in_batch_rejects_before_any_disposition(self):
        originals = self.seed(statuses=("pending", "held", "delivered", "unfinished", "consumed"))
        for original in originals:
            with self.subTest(status=original["status"]):
                before = snapshot(self.root)
                conflicting = dict(original["event"], body="different")
                request = [signal("missing"), conflicting]
                if original["sequence"] != 1:
                    request.insert(0, originals[0]["event"])
                with patch.object(wake, "_write", side_effect=AssertionError("changed queue")):
                    with self.assertRaises(wake.Refusal):
                        wake.consume_signal_events(self.root, request)
                self.assertEqual(snapshot(self.root), before)

    def test_archived_payload_conflict_rejects_pending_batch_member(self):
        original = wake.enqueue(self.root, signal())
        wake.consume_signal_events(self.root, [original["event"]])
        pending = wake.enqueue(self.root, signal(2))
        before = snapshot(self.root)
        with self.assertRaises(wake.Refusal):
            wake.consume_signal_events(self.root, [pending["event"], signal(body="changed")])
        with self.assertRaises(wake.Refusal):
            wake.enqueue(self.root, signal(body="changed"))
        self.assertEqual(snapshot(self.root), before)

    def test_each_durable_cut_reopens_and_exactly_replays_or_maintains(self):
        boundaries = ("mark-write", "mark-file-sync", "mark-replace", "mark-directory-sync",
                      "archive-parent-sync", "archive-write", "archive-file-sync", "archive-rename",
                      "archive-directory-sync", "prune-write", "prune-file-sync", "prune-replace",
                      "prune-directory-sync")
        for recovery in ("replay", "maintenance"):
            for boundary in boundaries:
                for when in ("before", "after"):
                    with self.subTest(recovery=recovery, boundary=boundary, when=when):
                        root = self.new_root("-".join((recovery, boundary, when)))
                        original = wake.enqueue(root, signal())
                        untouched = wake.enqueue(root, signal(2))
                        with DurableCut(root, boundary, when) as cut:
                            with self.assertRaises(InjectedCut):
                                wake.consume_signal_events(root, [original["event"]])
                        self.assertEqual(cut.hits, 1, "the intended boundary was not exercised")
                        visible = wake.lookup(root, wake.SIGNAL_SOURCE, "1")
                        self.assertEqual(visible["record"]["event"], original["event"])
                        self.assertEqual(visible["record"]["sequence"], original["sequence"])
                        was_pending = visible["record"]["status"] == "pending"
                        crossed = boundary not in ("mark-write", "mark-file-sync") and not (
                            boundary == "mark-replace" and when == "before")
                        self.assertEqual(was_pending, not crossed)
                        self.assertFalse((root / ".workshop.lock").exists())
                        self.assertFalse((root / "runs").exists())
                        self.assertEqual(list(root.rglob("*.tmp")), [])
                        if recovery == "maintenance":
                            result = wake.maintain(root)
                            self.assertEqual(result["status"], "maintained")
                            self.assertEqual(result["archived_events"], int(
                                crossed and visible["location"] == "active"))
                        expected_counts = counts(consumed=1) if was_pending else counts(already=1)
                        self.assertEqual(wake.consume_signal_events(root, [original["event"]]), expected_counts)
                        self.assert_consumed(root, original)
                        self.assertEqual(wake.active_records(root), [untouched])
                        self.assertEqual(wake.watermark(root), 2)
                        before = snapshot(root)
                        self.assertEqual(wake.consume_signal_events(root, [original["event"]]), counts(already=1))
                        self.assertEqual(snapshot(root), before)

    def test_corrupt_or_conflicting_tombstone_preserves_active_consumed_row(self):
        original = self.seed(statuses=("consumed",))[0]
        path = wake._archive_path(self.root, wake.SIGNAL_SOURCE, "1")
        path.parent.mkdir()
        valid = wake._consumed_archive_entry(original)
        variants = []
        for location, field, value in (
                (None, "schema", "unknown"), (None, "event_sha256", "0" * 64),
                (None, "extra", True), (None, "record", None),
                ("record", "sequence", 100), ("record", "sequence", True),
                ("record", "status", "pending"), ("record", "claimed_run", "fake-run"),
                ("terminal", "status", "replied"), ("terminal", "reply", "fake reply")):
            value_copy = copy.deepcopy(valid)
            target = value_copy if location is None else value_copy[location]
            target[field] = value
            variants.append(json.dumps(value_copy))
        conflict = copy.deepcopy(original)
        conflict["event"]["body"] = "different canonical payload"
        variants.append(json.dumps(wake._consumed_archive_entry(conflict)))
        variants += ["{", '{"schema":"a","schema":"b"}']
        for value in variants:
            for operation in ("consume", "maintain"):
                with self.subTest(value=value[:80], operation=operation):
                    path.write_text(value)
                    before = snapshot(self.root)
                    with self.assertRaises(wake.Refusal):
                        if operation == "consume":
                            wake.consume_signal_events(self.root, [original["event"]])
                        else:
                            wake.maintain(self.root)
                    self.assertEqual(snapshot(self.root), before)
                    self.assertEqual(wake.active_records(self.root), [original])


class UpgradeTests(unittest.TestCase):
    """Compatibility is against genuine prior source, not a rewritten v3 mock."""

    setUp = ConsumeTests.setUp
    new_root = ConsumeTests.new_root
    assert_consumed = ConsumeTests.assert_consumed

    @classmethod
    def setUpClass(cls):
        path = Path(__file__).parent / "fixtures" / "wake_v3.py"
        if hashlib.sha256(path.read_bytes()).hexdigest() != V3_SHA256:
            raise AssertionError("Frozen deployed v3 fixture changed")
        spec = importlib.util.spec_from_file_location("_consumption_wake_v3", path)
        cls.old = importlib.util.module_from_spec(spec)
        spec.loader.exec_module(cls.old)

    def legacy_queue(self, format_kind):
        (self.root / "state.json").unlink(missing_ok=True)
        original = self.old.enqueue(self.root, signal())
        envelope = json.loads((self.root / "events.json").read_text())
        envelope["next_sequence"] = 40
        if format_kind == "array":
            value = envelope["records"]
            (self.root / "state.json").write_text(json.dumps({
                "event_after_sequence": 12, "interactive": {"event_after_sequence": 39}}))
        else:
            value = dict(envelope, schema=format_kind)
        (self.root / "events.json").write_text(json.dumps(value))
        return original

    def test_upgrade_version_is_read_only_for_every_supported_format_and_absent(self):
        with patch.object(wake, "_locked", side_effect=AssertionError("read-only acquired lease")):
            self.assertEqual(wake.upgrade_version(self.root), "absent")
        self.assertEqual(list(self.root.iterdir()), [])
        original = wake.enqueue(self.root, signal())
        path = self.root / "events.json"
        for format_kind in ("array", wake.V2_SCHEMA, wake.PREVIOUS_SCHEMA, wake.SCHEMA):
            with self.subTest(format_kind=format_kind):
                value = [original] if format_kind == "array" else {
                    "schema": format_kind, "next_sequence": 2, "records": [original]}
                path.write_text(json.dumps(value))
                before = snapshot(self.root)
                with patch.object(wake, "_locked", side_effect=AssertionError("read-only acquired lease")):
                    self.assertEqual(wake.upgrade_version(self.root), format_kind)
                    self.assertEqual(wake.active_records(self.root), [original])
                    self.assertTrue(wake.pending(self.root))
                    self.assertEqual(wake.watermark(self.root), 1)
                    self.assertEqual(wake.inbox_summary(self.root)["pending_signal_bursts"], 1)
                self.assertEqual(snapshot(self.root), before)

    def test_all_current_writers_refuse_each_legacy_format_until_explicit_upgrade(self):
        operations = {
            "enqueue-new": lambda: wake.enqueue(self.root, signal(2)),
            "enqueue-replay": lambda: wake.enqueue(self.root, signal()),
            "claim": lambda: wake.claim(self.root, "run"),
            "delivered-noop": lambda: wake.delivered(self.root, "absent"),
            "unfinished-noop": lambda: wake.unfinished(self.root, "absent", "reason"),
            "recover-noop": lambda: wake.recover(self.root, set()),
            "consume": lambda: wake.consume_signal_events(self.root, [signal()]),
            "consume-noop": lambda: wake.consume_signal_events(self.root, []),
            "fence-current": lambda: wake.fence_current(self.root),
            "maintain": lambda: wake.maintain(self.root),
            "private-commit": lambda: wake._write(self.root, [], 40),
        }
        for format_kind in ("array", wake.V2_SCHEMA, wake.PREVIOUS_SCHEMA):
            with self.subTest(format_kind=format_kind):
                original = self.legacy_queue(format_kind)
                before = snapshot(self.root)
                for name, operation in operations.items():
                    with self.subTest(writer=name):
                        with self.assertRaisesRegex(wake.Refusal, "explicit queue upgrade"):
                            operation()
                        self.assertEqual(snapshot(self.root), before)
                (self.root / "PAUSED").touch()
                paused = snapshot(self.root)
                with self.assertRaisesRegex(wake.Refusal, "explicit queue upgrade"):
                    wake.archive(self.root)
                self.assertEqual(snapshot(self.root), paused)
                (self.root / "PAUSED").unlink()
                result = wake.upgrade(self.root)
                self.assertEqual(result, {"schema": wake.SCHEMA, "previous_schema": format_kind,
                                          "upgraded": True})
                self.assertEqual(wake.active_records(self.root), [original])
                self.assertEqual(wake.watermark(self.root), 39)
                self.assertEqual(wake.enqueue(self.root, signal(2))["sequence"], 40)
                # Recreate a genuine prior queue for the next matrix row.
                (self.root / "events.json").unlink()

    def test_absent_creation_and_idempotent_explicit_upgrade(self):
        self.assertEqual(wake.upgrade(self.root), {"schema": wake.SCHEMA,
                         "previous_schema": "absent", "upgraded": True})
        self.assertEqual(wake.upgrade_version(self.root), wake.SCHEMA)
        before = snapshot(self.root)
        self.assertEqual(wake.upgrade(self.root), {"schema": wake.SCHEMA,
                         "previous_schema": wake.SCHEMA, "upgraded": False})
        wake.fence_current(self.root)
        self.assertEqual(snapshot(self.root), before)
        self.assertEqual(wake.enqueue(self.root, signal())["sequence"], 1)
        other = self.new_root("fence")
        wake.fence_current(other)
        self.assertEqual(wake.upgrade_version(other), wake.SCHEMA)

    def test_frozen_v3_readers_and_writers_refuse_v4_including_empty_active_queue(self):
        original = self.old.enqueue(self.root, signal())
        self.assertTrue(self.old.pending(self.root))
        self.assertEqual(self.old.enqueue(self.root, signal()), original)
        wake.upgrade(self.root)
        operations = {
            "pending": lambda: self.old.pending(self.root),
            "watermark": lambda: self.old.watermark(self.root),
            "lookup": lambda: self.old.lookup(self.root, wake.SIGNAL_SOURCE, "1"),
            "active-records": lambda: self.old.active_records(self.root),
            "enqueue-new": lambda: self.old.enqueue(self.root, signal(2)),
            "enqueue-replay": lambda: self.old.enqueue(self.root, signal()),
            "claim": lambda: self.old.claim(self.root, "old-run"),
            "delivered": lambda: self.old.delivered(self.root, "old-run"),
            "unfinished": lambda: self.old.unfinished(self.root, "old-run", "reason"),
            "recover": lambda: self.old.recover(self.root, set()),
            "fence": lambda: self.old.fence_v3(self.root),
            "maintain": lambda: self.old.maintain(self.root),
        }
        for phase in ("pending", "consumed", "empty"):
            with self.subTest(phase=phase):
                if phase == "consumed":
                    with patch.object(wake, "_publish_archive", side_effect=InjectedCut("archive")):
                        with self.assertRaises(InjectedCut):
                            wake.consume_signal_events(self.root, [signal()])
                elif phase == "empty":
                    wake.consume_signal_events(self.root, [signal()])
                    self.assertEqual(wake.active_records(self.root), [])
                before = snapshot(self.root)
                for name, operation in operations.items():
                    with self.subTest(old_operation=name):
                        with self.assertRaises(self.old.Refusal):
                            operation()
                        self.assertEqual(snapshot(self.root), before)
                (self.root / "PAUSED").touch()
                paused = snapshot(self.root)
                with self.assertRaises(self.old.Refusal):
                    self.old.archive(self.root)
                self.assertEqual(snapshot(self.root), paused)
                (self.root / "PAUSED").unlink()
        self.assert_consumed(self.root, original)

    def test_unknown_torn_and_invalid_consumed_legacy_state_stays_unchanged(self):
        original = wake.enqueue(self.root, signal())
        path = self.root / "events.json"
        variants = ["{", '{"schema":"unknown","next_sequence":2,"records":[]}',
                    '{"schema":"mneme.workshop.events.v4","records":[]}']
        for schema in (wake.V2_SCHEMA, wake.PREVIOUS_SCHEMA):
            variants.append(json.dumps({"schema": schema, "next_sequence": 2,
                                        "records": [dict(original, status="consumed")]}))
        for value in variants:
            with self.subTest(value=value):
                path.write_text(value)
                before = snapshot(self.root)
                for operation in (wake.upgrade_version, wake.upgrade, wake.fence_current):
                    with self.assertRaises(wake.Refusal):
                        operation(self.root)
                    self.assertEqual(snapshot(self.root), before)


if __name__ == "__main__":
    unittest.main()
