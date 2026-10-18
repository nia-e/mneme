"""Disposable local outbox tests; no owners, providers, or live configuration."""
import copy
import json
import os
from pathlib import Path
import sys
import tempfile
import threading
import unittest
from unittest.mock import patch

sys.path.insert(0, str(Path(__file__).parent))
import host_files
import reader_worker
import recording_contract as contract
import registry_recording
import touchstone_handoff as handoff
from rollout_primitives import encoded, digest
from store_registry import NativeSelection, resolve

OWNER = "01ARZ3NDEKTSV4RRFFQ69G5FAV"
NODE = "01ARZ3NDEKTSV4RRFFQ69G5FAW"


def observation(text="We found a voice that feels like mine."):
    def ref(index):
        return {"ordinal": index, "line": index + 1, "byte_offset": index * 4096,
                "line_bytes": 4096, "ordinal_scope": "source_turn", "raw_line_sha256": digest(str(index).encode())}
    return {"schema": contract.OBSERVATION_SCHEMA, "status": "complete", "reason": "source_turn_complete",
            "evidence": [{"kind": "user_statement", "ref": ref(0), "content": [{"content_index": 0, "text": text}]}],
            "boundary": ref(1), "coverage": {"source_turn": "closed_verified", "prompt": "verified",
                "earlier_session_context": "omitted", "other_host_inputs": "omitted", "private_reasoning": "excluded",
                "sensitivity_review": "not_performed", "missing_results": 0, "omissions": {},
                "memory_delivery": "not_recorded", "public_evidence": {"mode": "all", "observed_records": 1,
                    "selected_records": 1, "omitted_records": 0}}}


class HandoffTests(unittest.TestCase):
    def setUp(self):
        self.temp = tempfile.TemporaryDirectory()
        self.addCleanup(self.temp.cleanup)
        self.root = Path(self.temp.name).resolve()
        snapshot = {"schema": "mneme.config-selection.v1", "digest": "a" * 64, "cwd": str(self.root),
            "config": {}, "project": {"configured": False, "root": None, "overlay": None},
            "active": [{"name": "shared", "store": {"id": OWNER,
                "owner": {"url": "http://127.0.0.1:1", "db": "user", "path": None},
                "capture": {"intent": "cross-workspace", "kinds": ["lesson", "episode", "possibility"],
                    "evidence_kinds": list(contract.KINDS), "required_tags": [], "allow_associations": True,
                    "max_summary_bytes": 512, "max_body_bytes": 2048}},
                "eligible": {"read": True, "write": True, "core": True, "context": True,
                             "recall": True, "record": True, "maintenance": False}}]}
        self.selected = NativeSelection(snapshot, str(self.root / "memory.toml"), str(self.root / "mnemed"))
        self.native = patch("store_registry.native_select", side_effect=lambda *a, **k: self.selected)
        self.native.start(); self.addCleanup(self.native.stop)
        self.config = {"schema": "mneme.codex-hooks.config.v13", "state_dir": self.root / "state",
            "project_root": self.root, "memory_scope": "registry", "memory_mode": "async", "recording_mode": "automatic",
            "frozen_selection": self.selected.to_dict()}
        self.event = {"hook_event_name": "PostToolUse", "session_id": "s", "turn_id": "next",
                      "cwd": str(self.root)}
        self.activate()

    def activate(self, turn="next", closed=False):
        directory = reader_worker._directory(self.config)
        host_files.ensure_state_directory(directory)
        path, lock = reader_worker._paths(self.config, "s")
        host_files.atomic_write(lock, b"")
        state = reader_worker._new()
        state["config_pin"] = registry_recording.config_digest(self.config)
        state["active"] = {"turn": turn, "closed": closed}
        reader_worker._write(path, state)

    def freeze(self, *, text=None, summary="A shared moment", body="This might matter to my sense of authorship.", related=(), key=None, effect=None):
        prompt, context = contract.prepare(observation() if text is None else observation(text),
                                           list(related), selected_stores=self.selected.recordable)
        self.assertIsNotNone(prompt, context)
        wire = {"kind": "touchstone_nomination", "summary": summary, "body": body,
                "evidence_ids": [context.bindings[0].evidence_id], "destination": "shared",
                "related_memory_ids": [t.opaque_id for t in context.association_bindings]}
        if effect is not None:
            wire.update(kind="review_nomination", effect=effect)
        nomination = contract.validate_answer({"proposal": wire}, context)["nomination"]
        return handoff.freeze(self.config, "s", "source", key or "b" * 64, nomination, context,
                              source_start_ref={"byte_offset": 0, "line_bytes": 10, "raw_line_sha256": "c" * 64})

    def state_path(self):
        return self.config["state_dir"] / "touchstone-handoff" / "state.json"

    def test_review_shares_outbox_with_touchstones_and_emission_is_not_execution(self):
        old = self.freeze()
        review = self.freeze(key="d"*64, effect={"kind":"core_creation","summary":"Draft core wording","body":""})
        self.assertEqual(review["schema"], handoff.REVIEW_ENVELOPE_SCHEMA)
        self.assertEqual(handoff.enqueue(self.config, old)["outcome"], "queued")
        self.assertEqual(handoff.enqueue(self.config, review)["outcome"], "queued")
        self.assertEqual(len(json.loads(self.state_path().read_text())["entries"]), 2)
        result = handoff.consume(self.config, self.event, 16000)
        self.assertEqual(result["outcome"], "emitted", result)
        self.assertEqual(result["ids"], [old["id"], review["id"]])
        self.assertIn('"kind":"core_creation"', result["context"])
        self.assertIn("not approved meaning or native requests", result["context"])
        self.assertIn("Old ID-only operations do not acquire atomic approval protection", result["context"])
        self.assertIn("not read, approved or saved", result["context"])
        self.assertNotIn("service_config", result["context"])
        self.assertEqual(handoff.enqueue(self.config, review)["status"], "emitted")
        self.assertEqual(handoff.consume(self.config, self.event, 16000)["outcome"], "empty")

    def test_review_effect_target_binding_and_conflicting_source_replay(self):
        related = [{"db_id":OWNER,"id":NODE,"kind":"semantic","summary":"Inspected source note"}]
        effect = {"kind":"forget","target":"overlap001"}
        review = self.freeze(related=related, effect=effect)
        self.assertEqual(review["effect"]["targets"][0]["id"], NODE)
        self.assertEqual(handoff.enqueue(self.config, review)["outcome"], "queued")
        changed = self.freeze(related=related, effect={"kind":"core_membership","target":"overlap001","membership":"promote"})
        self.assertEqual(handoff.enqueue(self.config, changed)["outcome"], "refused")
        for mutate in (lambda value:value["effect"]["targets"][0].update(id=OWNER),
                       lambda value:value["effect"].update(approved=True),
                       lambda value:value.update(schema=handoff.ENVELOPE_SCHEMA)):
            broken = copy.deepcopy(review)
            mutate(broken)
            broken["id"] = handoff._identity(broken)
            with self.assertRaises(ValueError):
                handoff.validate_envelope(broken)
        result = handoff.consume(self.config, self.event, 16000)
        self.assertEqual(result["outcome"], "emitted")
        self.assertIn(NODE, result["context"])
        self.assertIn('"kind":"forget"', result["context"])

    def test_review_keeps_complete_proposal_pending_when_boundary_budget_is_too_small(self):
        review = self.freeze(effect={"kind":"core_creation","summary":"Draft", "body":"tentative "*100})
        handoff.enqueue(self.config, review)
        before = self.state_path().read_bytes()
        result = handoff.consume(self.config, self.event, 100)
        self.assertEqual(result["outcome"], "deferred")
        self.assertEqual(self.state_path().read_bytes(), before)
        result = handoff.consume(self.config, self.event, 16000)
        self.assertEqual(result["outcome"], "emitted")
        self.assertIn(review["effect"]["body"], result["context"])

    def test_restart_emit_once_and_compact_not_approved(self):
        value = self.freeze()
        self.assertEqual(handoff.enqueue(self.config, value)["outcome"], "queued")
        self.assertEqual(handoff.enqueue(copy.deepcopy(self.config), copy.deepcopy(value))["outcome"], "replay")
        result = handoff.consume(copy.deepcopy(self.config), self.event, 4096)
        self.assertEqual(result["outcome"], "emitted", result)
        self.assertEqual(result["ids"], [value["id"]])
        self.assertIn("not read, approved or saved", result["context"])
        self.assertNotIn("service_config", result["context"])
        saved = json.loads(self.state_path().read_text())
        self.assertEqual(saved["entries"][0]["status"], "emitted")
        self.assertNotIn("envelope", saved["entries"][0])
        self.assertEqual(handoff.enqueue(self.config, value)["status"], "emitted")
        self.assertEqual(handoff.consume(self.config, self.event, 4096)["outcome"], "empty")

    def test_next_turn_only_and_compaction_does_not_drop_pending(self):
        value = self.freeze(); handoff.enqueue(self.config, value)
        self.activate("source")
        self.assertEqual(handoff.consume(self.config, {**self.event, "turn_id": "source"}, 4096)["outcome"], "deferred")
        self.assertEqual(handoff.consume(self.config, self.event, 4096)["reason"], "no_current_open_turn")
        self.activate("next")
        path, _ = reader_worker._paths(self.config, "s")
        state = json.loads(path.read_text()); state["context_generation"] += 1
        reader_worker._write(path, state)
        self.assertEqual(handoff.consume(self.config, {**self.event, "hook_event_name": "UserPromptSubmit"}, 4096)["outcome"], "emitted")

    def test_closed_background_other_session_and_event_do_not_emit(self):
        value = self.freeze(); handoff.enqueue(self.config, value)
        self.activate(closed=True)
        self.assertEqual(handoff.consume(self.config, self.event, 4096)["outcome"], "deferred")
        for changes in ({"agent_id": "worker"}, {"hook_event_name": "Stop"}, {"session_id": "other"}):
            result = handoff.consume(self.config, {**self.event, **changes}, 4096)
            self.assertNotEqual(result["outcome"], "emitted", result)
            self.assertNotIn("context", result)

    def test_budget_keeps_whole_meaning_pending_then_emits(self):
        value = self.freeze(body="A meaningful whole. " * 30)
        handoff.enqueue(self.config, value)
        result = handoff.consume(self.config, self.event, 64)
        self.assertEqual(result["outcome"], "deferred")
        self.assertEqual(result["pending_count"], 1)
        self.assertNotIn("context", result)
        result = handoff.consume(self.config, self.event, 4096)
        self.assertEqual(result["outcome"], "emitted", result)
        self.assertIn(value["body"], result["context"])

    def test_exact_evidence_omitted_whole_when_packet_needs_space(self):
        value = self.freeze(text="Bounded scene. " * 130)
        self.assertIsNotNone(value["evidence"][0]["text"])
        exact = handoff._render([value]); compact = handoff._render([value], texts=False)
        self.assertGreater(len(exact.encode()), len(compact.encode()))
        handoff.enqueue(self.config, value)
        result = handoff.consume(self.config, self.event, len(compact.encode()))
        self.assertEqual(result["outcome"], "emitted", result)
        self.assertNotIn("Bounded scene.", result["context"])
        self.assertIn("exact source coordinates only", result["context"])
        self.assertLessEqual(len(result["context"].encode()), len(compact.encode()))

    def test_unicode_max_meaning_and_many_related_never_truncated(self):
        cards = [{"id": "01ARZ3NDEKTSV4RRFFQ69G5" + suffix, "db_id": OWNER,
                  "kind": "semantic", "summary": "Related " + suffix} for suffix in ("A00", "A01", "A02", "A03", "A04", "A05")]
        # Max prose alone fits whole. Additional references may exceed the
        # real hook envelope: reject, do not silently trim or park forever.
        maximum = self.freeze(summary="é" * 256, body="é" * 1024)
        self.assertLessEqual(len(handoff._render([maximum], texts=False).encode()), 4096)
        with self.assertRaisesRegex(ValueError, "nomination_delivery_cap"):
            self.freeze(summary="é" * 256, body="é" * 1024, related=cards)
        value = self.freeze(summary="é" * 128, body="é" * 512, related=cards)
        self.assertEqual(len(value["related"]), 6)
        handoff.enqueue(self.config, value)
        result = handoff.consume(self.config, self.event, 4096)
        if result["outcome"] == "emitted":
            self.assertIn(value["body"], result["context"])
            self.assertIn(value["summary"], result["context"])
        else:
            self.assertEqual(result["outcome"], "deferred")
            self.assertNotIn("context", result)
        self.assertEqual(handoff.consume(self.config, self.event, 16000)["outcome"], "emitted" if result["outcome"] != "emitted" else "empty")

    def test_four_citation_maximum_refuses_permanently_undeliverable_packet(self):
        source = observation()
        first = source["evidence"][0]
        for index in range(1, 4):
            item = copy.deepcopy(first)
            item["ref"].update(ordinal=index, line=index + 1, byte_offset=index * 4096,
                               raw_line_sha256=digest(str(index).encode()))
            source["evidence"].append(item)
        source["boundary"].update(ordinal=4, line=5, byte_offset=4 * 4096)
        source["coverage"]["public_evidence"].update(observed_records=4, selected_records=4)
        prompt, context = contract.prepare(source, selected_stores=self.selected.recordable)
        self.assertIsNotNone(prompt, context)
        nomination = contract.validate_answer({"proposal": {"kind": "touchstone_nomination",
            "summary": "x" * 512, "body": "y" * 2048, "evidence_ids": [b.evidence_id for b in context.bindings],
            "related_memory_ids": [], "destination": "shared"}}, context)["nomination"]
        with self.assertRaisesRegex(ValueError, "nomination_delivery_cap"):
            handoff.freeze(self.config, "s", "source", "b" * 64, nomination, context)
        self.assertFalse(self.state_path().exists())

    def test_policy_revocation_owner_swap_and_missing_selection_refuse(self):
        value = self.freeze(); handoff.enqueue(self.config, value)
        original = self.selected
        for update in (lambda s: s["active"][0]["eligible"].update(context=False),
                       lambda s: s["active"][0]["eligible"].update(record=False),
                       lambda s: s["active"][0]["eligible"].update(read=False),
                       lambda s: s["active"][0]["store"].update(id=NODE),
                       lambda s: s.update(active=[])):
            snap = copy.deepcopy(original.snapshot); update(snap)
            self.selected = NativeSelection(snap, original.registry_path, original.native_cli)
            self.assertEqual(handoff.consume(self.config, self.event, 4096)["outcome"], "refused")
        self.selected = original
        for field, setting in (("recording_mode", "off"), ("memory_mode", "checkpoint")):
            config = {**self.config, field: setting}
            self.assertEqual(handoff.consume(config, self.event, 4096)["outcome"], "refused")
        self.assertEqual(handoff.consume(self.config, {**self.event, "cwd": str(self.root / "other")}, 4096)["outcome"], "suppressed")
        self.assertEqual(handoff.consume(self.config, self.event, 4096)["outcome"], "emitted")

    def test_cancel_marker_suppresses_without_body_or_approval(self):
        value = self.freeze(); handoff.enqueue(self.config, value)
        from recording_jobs import _cancel_path
        marker = _cancel_path(self.config, "s")
        host_files.ensure_state_directory(marker.parent)
        host_files.atomic_write(marker, encoded({"token": "d" * 64, "turn_id": "source"}))
        result = handoff.consume(self.config, self.event, 4096)
        self.assertEqual(result["outcome"], "suppressed", result)
        self.assertEqual(result["suppressed_count"], 1)
        self.assertNotIn("context", result)
        self.assertNotIn("envelope", json.loads(self.state_path().read_text())["entries"][0])

    def test_source_key_conflict_and_unknown_torn_formats_refused(self):
        value = self.freeze(); handoff.enqueue(self.config, value)
        changed = self.freeze(body="Different meaning, same source key.")
        self.assertEqual(handoff.enqueue(self.config, changed)["outcome"], "refused")
        for raw in (b'{"schema":', encoded({"schema": "future", "entries": []}), b'{"schema":1,"schema":2}'):
            host_files.atomic_write(self.state_path(), raw)
            self.assertEqual(handoff.consume(self.config, self.event, 4096)["outcome"], "refused")
            self.assertEqual(handoff.enqueue(self.config, value)["outcome"], "refused")
            self.assertEqual(self.state_path().read_bytes(), raw)

    def test_absent_missing_and_unsafe_paths_distinct_from_empty(self):
        self.assertEqual(handoff.consume(self.config, self.event, 4096)["outcome"], "absent")
        value = self.freeze(); handoff.enqueue(self.config, value)
        self.state_path().unlink()
        self.assertEqual(handoff.consume(self.config, self.event, 4096)["outcome"], "unavailable")
        self.assertEqual(handoff.enqueue(self.config, value)["outcome"], "unavailable")
        other = self.root / "elsewhere"; other.write_text("not state")
        self.state_path().symlink_to(other)
        self.assertEqual(handoff.enqueue(self.config, value)["outcome"], "unavailable")
        self.assertEqual(other.read_text(), "not state")

    def test_symlink_directory_and_unprivate_state_refused(self):
        value = self.freeze()
        target = self.root / "target"; target.mkdir(mode=0o700)
        directory = self.config["state_dir"] / "touchstone-handoff"
        directory.symlink_to(target)
        self.assertEqual(handoff.enqueue(self.config, value)["outcome"], "unavailable")
        self.assertFalse((target / "state.json").exists())
        directory.unlink(); handoff.enqueue(self.config, value)
        if os.name != "nt":
            self.state_path().chmod(0o644)
            self.assertEqual(handoff.consume(self.config, self.event, 4096)["outcome"], "unavailable")

    def test_capacity_does_not_evict_pending_or_replay_tombstones(self):
        value = self.freeze(); handoff.enqueue(self.config, value)
        before = self.state_path().read_bytes()
        limits = handoff._limits()
        with patch.object(handoff, "_limits", return_value=(len(before), *limits[1:])):
            second = self.freeze(key="e" * 64)
            self.assertEqual(handoff.enqueue(self.config, second)["outcome"], "capacity")
            self.assertEqual(self.state_path().read_bytes(), before)
        handoff.consume(self.config, self.event, 4096)
        self.assertEqual(handoff.enqueue(self.config, value)["status"], "emitted")

    def test_expiration_compacts_explicitly_and_cannot_replay_old_envelope(self):
        value = self.freeze(); handoff.enqueue(self.config, value)
        with patch.object(handoff.time, "time", return_value=value["frozen_at"] + handoff._limits()[3] + 1):
            self.assertEqual(handoff.enqueue(self.config, value)["outcome"], "expired")
            result = handoff.consume(self.config, self.event, 4096)
            self.assertEqual(result["expired_count"], 1)
            self.assertEqual(result["outcome"], "empty")
            self.assertEqual(json.loads(self.state_path().read_text())["expired"], 1)

    def test_concurrent_consumers_emit_at_most_once(self):
        value = self.freeze(); handoff.enqueue(self.config, value)
        outputs = []
        threads = [threading.Thread(target=lambda: outputs.append(handoff.consume(self.config, self.event, 4096))) for _ in range(8)]
        for thread in threads: thread.start()
        for thread in threads: thread.join()
        self.assertEqual(sum(result["outcome"] == "emitted" for result in outputs), 1, outputs)
        self.assertTrue(all("context" not in result for result in outputs if result["outcome"] != "emitted"))

    def test_atomic_publication_failure_never_claims_emitted(self):
        value = self.freeze(); handoff.enqueue(self.config, value)
        with patch.object(host_files, "atomic_write", side_effect=OSError("simulated failure")):
            result = handoff.consume(self.config, self.event, 4096)
        self.assertEqual(result["outcome"], "unavailable")
        self.assertNotIn("context", result)
        self.assertEqual(handoff.consume(self.config, self.event, 4096)["outcome"], "emitted")

    def test_obvious_credentials_are_not_copied_as_source_snippets(self):
        text = "Synthetic bearer " + "A" * 24 + " appeared in this source record."
        value = self.freeze(text=text)
        self.assertEqual(value["evidence"][0]["text_coverage"], "coordinates_only")
        self.assertNotIn("A" * 24, encoded(value).decode())
        self.assertEqual(handoff.enqueue(self.config, value)["outcome"], "queued")

    def test_reader_directory_symlink_does_not_supply_foreground_authority(self):
        value = self.freeze(); handoff.enqueue(self.config, value)
        directory = reader_worker._directory(self.config)
        moved = directory.with_name("reader-moved")
        directory.rename(moved); directory.symlink_to(moved, target_is_directory=True)
        self.assertEqual(handoff.consume(self.config, self.event, 4096)["outcome"], "unavailable")
        self.assertEqual(json.loads(self.state_path().read_text())["entries"][0]["status"], "pending")

    def test_direct_call_cannot_enlarge_hook_packet_ceiling(self):
        value = self.freeze(text="Bounded scene. " * 130, body="This matters. " * 80)
        self.assertGreater(len(handoff._render([value]).encode()), 4096)
        handoff.enqueue(self.config, value)
        result = handoff.consume(self.config, self.event, 1_000_000)
        self.assertEqual(result["outcome"], "emitted", result)
        self.assertLessEqual(len(result["context"].encode()), 4096)
        self.assertIn("exact source coordinates only", result["context"])

    def test_unknown_directory_debris_refuses_further_publication(self):
        value = self.freeze(); handoff.enqueue(self.config, value)
        before = self.state_path().read_bytes()
        extra = self.state_path().parent / ".mneme-publish-interrupted"
        host_files.atomic_write(extra, b"incomplete prior publication")
        self.assertEqual(handoff.consume(self.config, self.event, 4096)["outcome"], "refused")
        self.assertEqual(handoff.enqueue(self.config, value)["outcome"], "refused")
        self.assertEqual(self.state_path().read_bytes(), before)
        self.assertTrue(extra.exists())

    def test_v12_uses_explicit_read_permission(self):
        service = self.root / "service.json"
        service.write_text(json.dumps({"mode": "connect", "database_name": "user", "database_path": "/srv/mneme/user.db",
                                      "url": "http://127.0.0.1:18766/"}))
        registry = self.root / "stores.json"
        entry = {"name": "shared", "service_config": str(service), "db_alias": "user", "database_path": "/srv/mneme/user.db",
            "db_id": OWNER, "purpose": "Shared continuity.", "read": {"enabled": True},
            "record": {"enabled": True, "kinds": ["lesson"], "evidence_kinds": list(contract.KINDS),
                "required_tags": [], "allow_associations": True, "max_summary_bytes": 512, "max_body_bytes": 2048},
            "maintenance": {"enabled": False}}
        registry.write_text(json.dumps({"schema": "mneme.stores.v1", "stores": [entry]}))
        self.selected = resolve(self.root, registry_path=registry)
        self.config.update(schema="mneme.codex-hooks.config.v12", frozen_selection=self.selected.to_dict())
        self.activate()
        value = self.freeze(); self.assertEqual(handoff.enqueue(self.config, value)["outcome"], "queued")
        self.assertEqual(handoff.consume(self.config, self.event, 4096)["outcome"], "emitted")
        entry["read"]["enabled"] = False
        registry.write_text(json.dumps({"schema": "mneme.stores.v1", "stores": [entry]}))
        self.selected = resolve(self.root, registry_path=registry)
        self.config["frozen_selection"] = self.selected.to_dict()
        with self.assertRaisesRegex(ValueError, "destination_handoff_denied"):
            self.freeze()


if __name__ == "__main__":
    unittest.main()
