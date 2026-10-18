"""Disposable source-shaped nominations; no installed Claude, providers or owners."""
import copy
import json
from pathlib import Path
import sys
import tempfile
import unittest
from unittest.mock import patch

sys.path.insert(0, str(Path(__file__).parent))
import capture_source
import host_files
import reader_worker
import recording_contract as contract
import registry_recording
import touchstone_handoff as handoff
from rollout_primitives import encoded, digest
from store_registry import NativeSelection

OWNER = "01ARZ3NDEKTSV4RRFFQ69G5FAV"
OTHER = "01ARZ3NDEKTSV4RRFFQ69G5FAW"


def observation():
    def ref(ordinal):
        return {"ordinal": ordinal, "line": ordinal + 1, "ordinal_scope": "source_window",
                "byte_offset": 10000 + ordinal * 4096, "line_bytes": 4096,
                "raw_line_sha256": digest(str(ordinal).encode()), "item_id": "uuid-" + str(ordinal)}
    # Two logical blocks share the exact final physical record. Ordinals are not
    # reassigned to make evidence indices look like JSONL line coordinates.
    return {"schema": contract.CAPTURE_OBSERVATION_SCHEMA, "source_host": "claude_code",
        "status": "observed", "reason": "source_snapshot_ready", "evidence": [
            {"kind": "user_statement", "ref": ref(0), "evidence_index": 0,
             "source_field": "/message/content", "content": [{"content_index": 0, "text": "Let's keep this moment."}]},
            {"kind": "assistant_assertion", "phase": "final_answer", "ref": ref(1), "evidence_index": 8,
             "source_field": "/message/content/1/text", "content": [{"content_index": 1, "text": "A shared voice."}]},
            {"kind": "assistant_assertion", "phase": "final_answer", "ref": ref(1), "evidence_index": 9,
             "source_field": "/message/content/2/text", "content": [{"content_index": 2, "text": "With room to change."}]}],
        "boundary": ref(1), "coverage": {"source_turn": "observed_prefix",
            "turn_finality": "unproven", "remaining_transcript": "unknown",
            "snapshot": {"schema": "mneme.capture-source-snapshot.v1", "trigger": "next_prompt",
                "observed_through": ref(1), "observed_sha256": "a" * 64, "latest_stop": "matched",
                "trailing_partial_line": False, "unmatched_calls": 0}, "prompt": "verified", "prompt_authority": "unverified",
            "earlier_session_context": "omitted", "other_host_inputs": "omitted", "private_reasoning": "excluded",
            "sensitivity_review": "not_performed", "missing_results": 0, "omissions": {},
            "memory_delivery": "not_recorded", "public_evidence": {"mode": "all", "observed_records": 3,
                "selected_records": 3, "omitted_records": 0}}}


def legacy_envelope():
    """Canonical Codex v1 fixture, hashed against genuine pre-F1 module bytes."""
    ref = {"ordinal": 0, "line": 1, "byte_offset": 0, "line_bytes": 100,
           "ordinal_scope": "source_turn", "raw_line_sha256": "a" * 64}
    value = {"schema": handoff.ENVELOPE_SCHEMA, "frozen_at": 1, "session_id": "s", "source_turn": "source",
        "source_key": "b" * 64, "source_start_ref": None,
        "source_boundary_ref": {**ref, "ordinal": 1, "line": 2, "byte_offset": 100},
        "config_digest": "c" * 64, "selection_digest": "d" * 64, "workspace": "/fixture/workspace",
        "owner": {"name": "shared", "db_id": OWNER, "db_alias": "user", "database_path": "",
                  "service_config": "/fixture/memory.toml", "store_digest": "e" * 64},
        "summary": "A moment", "body": "Meaning is authored, not inferred.",
        "evidence": [{"evidence_id": "e001", "kind": "user_statement", "source_ref": ref,
            "source_field": "content[0].text", "rendering": "exact_text", "text_sha256": digest(b"Hello."),
            "text": "Hello.", "text_coverage": "exact"}], "related": [],
        "coverage": {"source_turn": "closed_verified", "public_evidence": {"mode": "all", "observed_records": 1,
            "selected_records": 1, "omitted_records": 0}, "earlier_context": "omitted"}, "cancel_token": ""}
    value["id"] = handoff._identity(value)
    return value


class ClaudeHandoffTests(unittest.TestCase):
    def setUp(self):
        self.temp = tempfile.TemporaryDirectory(); self.addCleanup(self.temp.cleanup)
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
        native = patch("store_registry.native_select", side_effect=lambda *a, **k: self.selected)
        native.start(); self.addCleanup(native.stop)
        self.config = {"schema": "mneme.claude-hooks.config.v1", "source_host": "claude_code", "assessor": "claude_code",
            "state_dir": self.root / "state", "project_root": self.root, "memory_scope": "registry",
            "memory_mode": "async", "recording_mode": "automatic", "frozen_selection": self.selected.to_dict()}
        self.event = {"hook_event_name": "UserPromptSubmit", "session_id": "s", "turn_id": "next",
                      "source_host": "claude_code", "agent_type": "custom-root", "cwd": str(self.root)}
        self.activate()

    def activate(self, turn="next", closed=False):
        directory = reader_worker._directory(self.config)
        host_files.ensure_state_directory(directory)
        path, lock = reader_worker._paths(self.config, "s")
        host_files.atomic_write(lock, b"")
        state = reader_worker._new(self.config)
        state["config_pin"] = registry_recording.config_digest(self.config)
        state["active"] = {"turn": turn, "closed": closed}
        reader_worker._write(path, state)

    def freeze(self, *, source=None, summary="A shared voice", body="This scene might matter.", key="b" * 64, citations=None, effect=None):
        prompt, context = contract.prepare(source or observation(), selected_stores=self.selected.recordable)
        self.assertIsNotNone(prompt, context)
        wire = {"kind":"touchstone_nomination", "summary":summary, "body":body,
                "evidence_ids":citations or [b.evidence_id for b in context.bindings],
                "destination":"shared", "related_memory_ids":[]}
        if effect is not None:
            wire.update(kind="review_nomination", effect=effect)
        nomination = contract.validate_answer({"proposal":wire}, context)["nomination"]
        return handoff.freeze(self.config, "s", "source", key, nomination, context)

    def path(self):
        return capture_source.directory(self.config, "touchstone-nominations") / "state.json"

    def test_review_handoff_preserves_observed_snapshot_coverage_not_finality(self):
        value = self.freeze(effect={"kind":"core_creation","summary":"Draft core note","body":""})
        self.assertEqual(value["schema"], handoff.CLAUDE_REVIEW_ENVELOPE_SCHEMA)
        self.assertEqual(value["coverage"]["source_turn"], "observed_prefix")
        self.assertEqual(value["coverage"]["turn_finality"], "unproven")
        self.assertEqual(value["coverage"]["remaining_transcript"], "unknown")
        self.assertEqual(value["coverage"]["snapshot"], observation()["coverage"]["snapshot"])
        self.assertEqual(handoff.enqueue(self.config, value)["outcome"], "queued")
        result = handoff.consume(self.config, self.event, 16000)
        self.assertEqual(result["outcome"], "emitted", result)
        self.assertIn('"kind":"core_creation"', result["context"])
        self.assertIn('"source_turn":"observed_prefix"', result["context"])
        self.assertIn('"turn_finality":"unproven"', result["context"])
        self.assertNotIn('"source_turn":"closed_verified"', result["context"])
        self.assertEqual(handoff.enqueue(self.config, value)["status"], "emitted")

    def test_source_shaped_replay_and_later_turn_emits_once_not_approval(self):
        value = self.freeze()
        self.assertEqual(value["schema"], handoff.CLAUDE_ENVELOPE_SCHEMA)
        self.assertEqual(value["source_host"], "claude_code")
        self.assertIsNone(value["source_start_ref"])
        self.assertEqual(value["source_boundary_ref"], value["evidence"][-1]["source_ref"])
        self.assertEqual(value["evidence"][-1]["source_ref"], value["evidence"][-2]["source_ref"])
        self.assertEqual(value["evidence"][-1]["source_ref"]["ordinal"], 1)
        self.assertNotIn("evidence_index", value["evidence"][-1]["source_ref"])
        self.assertEqual(handoff.enqueue(self.config, value)["outcome"], "queued")
        self.assertEqual(handoff.enqueue(copy.deepcopy(self.config), copy.deepcopy(value))["outcome"], "replay")
        self.assertEqual(self.path(), self.config["state_dir"] / "capture/claude_code/touchstone-nominations/state.json")
        self.assertFalse((self.config["state_dir"] / "touchstone-handoff").exists())
        self.assertEqual(json.loads(self.path().read_text())["schema"], handoff.CLAUDE_STATE_SCHEMA)
        result = handoff.consume(self.config, self.event, 4096)
        self.assertEqual(result["outcome"], "emitted", result)
        self.assertIn('"host":"claude_code"', result["context"])
        self.assertIn('"ordinal_scope":"source_window"', result["context"])
        self.assertIn('"field":"/message/content/2/text"', result["context"])
        self.assertIn("not read, approved or saved", result["context"])
        self.assertNotIn("service_config", result["context"])
        self.assertLessEqual(len(result["context"].encode()), 4096)
        self.assertEqual(handoff.consume(self.config, self.event, 4096)["outcome"], "empty")
        self.assertEqual(handoff.enqueue(self.config, value)["status"], "emitted")

    def test_pending_legacy_claude_nomination_keeps_original_closed_coverage(self):
        value=self.freeze()
        value['schema']=handoff.LEGACY_CLAUDE_ENVELOPE_SCHEMA
        value['coverage']={k:v for k,v in value['coverage'].items()
                           if k not in ('snapshot','turn_finality','remaining_transcript')}
        value['coverage']['source_turn']='closed_verified'
        value['id']=handoff._identity(value)
        self.assertEqual(handoff.validate_envelope(value),value)
        self.assertEqual(handoff.enqueue(self.config,value)['outcome'],'queued')
        result=handoff.consume(self.config,self.event,4096)
        self.assertEqual(result['outcome'],'emitted',result)
        self.assertIn('"source_turn":"closed_verified"',result['context'])
        self.assertNotIn('"snapshot"',result['context'])
        self.assertEqual(handoff.consume(self.config,self.event,4096)['outcome'],'empty')

    def test_host_format_and_raw_source_fences(self):
        value = self.freeze()
        for edit in (lambda v: v.update(source_host="codex"), lambda v: v.update(schema=handoff.ENVELOPE_SCHEMA),
            lambda v: v.update(source_start_ref={"byte_offset": 0, "line_bytes": 1, "raw_line_sha256": "c" * 64}),
            lambda v: v["source_boundary_ref"].update(ordinal_scope="source_turn"),
            lambda v: v["evidence"][0]["source_ref"].update(ordinal_scope="transcript"),
            lambda v: v["evidence"][0]["source_ref"].update(evidence_index=0),
            lambda v: v["evidence"][-1]["source_ref"].update(raw_line_sha256="f" * 64),
            lambda v: v["evidence"][0]["source_ref"].update(byte_offset=v["source_boundary_ref"]["byte_offset"]),
            lambda v: v["evidence"][0].update(source_field="content[0].text"),
            lambda v: v["evidence"][0].update(rendering="future"),
            lambda v: v["evidence"][0].update(source_field="/message/content/01/text"),
            lambda v: v["evidence"][0].update(source_field="/message/content/~2"),
            lambda v: v["evidence"][-1].update(source_field=v["evidence"][-2]["source_field"])):
            changed = copy.deepcopy(value); edit(changed); changed["id"] = handoff._identity(changed)
            with self.subTest(value=changed):
                with self.assertRaises(ValueError): handoff.validate_envelope(changed)
        codex = {**self.config, "source_host": "codex", "schema": "mneme.codex-hooks.config.v13"}
        self.assertEqual(handoff.enqueue(codex, value)["outcome"], "refused")
        self.assertFalse((self.config["state_dir"] / "touchstone-handoff").exists())
        self.assertEqual(handoff.enqueue(self.config, legacy_envelope())["outcome"], "refused")
        for event in ({**self.event, "source_host": "codex"}, {k: v for k, v in self.event.items() if k != "source_host"},
                      {**self.event, "source_host": "future"}, {**self.event, "agent_id": "child"}):
            self.assertEqual(handoff.consume(self.config, event, 4096)["outcome"], "skipped")

    def test_tool_fields_and_same_line_anchor_consistency(self):
        source = observation()
        call = {"kind": "tool_call", "type": "tool_use", "call_id": "call", "name": "Read", "input": {"path": "fixture"},
                "evidence_index": 1, "source_field": "/message/content/0", "ref": copy.deepcopy(source["evidence"][0]["ref"])}
        result = {"kind": "tool_result", "type": "tool_result", "call_id": "call", "output": "Fixture output.",
                  "evidence_index": 2, "source_field": "/message/content/1/content", "ref": copy.deepcopy(call["ref"])}
        source["evidence"][0]["source_field"] = "/message/content/2/text"
        source["evidence"][1:1] = [call, result]
        source["coverage"]["public_evidence"].update(observed_records=5, selected_records=5)
        value = self.freeze(source=source, citations=["e001", "t001_call", "t001_result", "e004"])
        self.assertEqual(handoff.validate_envelope(value), value)
        for edit in (lambda v: v["evidence"][1].update(source_field="/message/content/0/content"),
                     lambda v: v["evidence"][2].update(source_field="/message/content/1"),
                     lambda v: v["evidence"][1].update(rendering="exact_text"),
                     lambda v: v["evidence"][1]["source_ref"].update(raw_line_sha256="f" * 64),
                     lambda v: v["evidence"][1]["source_ref"].update(byte_offset=10001)):
            changed = copy.deepcopy(value); edit(changed); changed["id"] = handoff._identity(changed)
            with self.assertRaises(ValueError): handoff.validate_envelope(changed)

    def test_unknown_torn_and_crosshost_state_unchanged(self):
        value = self.freeze(); handoff.enqueue(self.config, value)
        for raw in (b'{"schema":', encoded({"schema": "future", "entries": []}), b'{"schema":1,"schema":2}',
                    encoded({"schema": handoff.STATE_SCHEMA, "entries": [], "expired": 0, "emitted": 0, "suppressed": 0})):
            host_files.atomic_write(self.path(), raw)
            self.assertEqual(handoff.consume(self.config, self.event, 4096)["outcome"], "refused")
            self.assertEqual(handoff.enqueue(self.config, value)["outcome"], "refused")
            self.assertEqual(self.path().read_bytes(), raw)

    def test_current_turn_closed_turn_cancel_and_policy_change(self):
        value = self.freeze(); handoff.enqueue(self.config, value)
        self.activate("source")
        self.assertEqual(handoff.consume(self.config, {**self.event, "turn_id": "source"}, 4096)["outcome"], "deferred")
        self.activate(closed=True)
        self.assertEqual(handoff.consume(self.config, self.event, 4096)["outcome"], "deferred")
        self.activate()
        original = self.selected
        for update in (lambda s: s["active"][0]["eligible"].update(context=False),
                       lambda s: s["active"][0]["eligible"].update(record=False),
                       lambda s: s["active"][0]["eligible"].update(read=False),
                       lambda s: s["active"][0]["store"].update(id=OTHER), lambda s: s.update(active=[])):
            snap = copy.deepcopy(original.snapshot); update(snap)
            self.selected = NativeSelection(snap, original.registry_path, original.native_cli)
            self.assertEqual(handoff.consume(self.config, self.event, 4096)["outcome"], "refused")
        self.selected = original
        from recording_jobs import _cancel_path
        marker = _cancel_path(self.config, "s"); host_files.ensure_state_directory(marker.parent)
        host_files.atomic_write(marker, encoded({"token": "f" * 64, "turn_id": "source"}))
        result = handoff.consume(self.config, self.event, 4096)
        self.assertEqual(result["outcome"], "suppressed", result)
        self.assertNotIn("context", result)
        self.assertNotIn("envelope", json.loads(self.path().read_text())["entries"][0])

    def test_capacity_expiry_and_conflicting_replay(self):
        value = self.freeze(); self.assertEqual(handoff.enqueue(self.config, value)["outcome"], "queued")
        self.assertEqual(handoff.enqueue(self.config, self.freeze(body="Different meaning."))["outcome"], "refused")
        raw = self.path().read_bytes(); limits = handoff._limits()
        with patch.object(handoff, "_limits", return_value=(len(raw), *limits[1:])):
            self.assertEqual(handoff.enqueue(self.config, self.freeze(key="f" * 64))["outcome"], "capacity")
        self.assertEqual(self.path().read_bytes(), raw)
        with patch.object(handoff.time, "time", return_value=value["frozen_at"] + limits[3] + 1):
            self.assertEqual(handoff.enqueue(self.config, value)["outcome"], "expired")
            self.assertEqual(handoff.consume(self.config, self.event, 4096)["expired_count"], 1)

    def test_exact_4096_delivery_admission_and_no_meaning_truncation(self):
        # Search prose length, not a moving quota: every additional ASCII byte
        # changes the compact whole packet by exactly one byte.
        source = observation()
        for item in source["evidence"]:
            item["content"][0]["text"] = "A whole scene. " * 50
        base = self.freeze(source=source)
        available = 4096 - len(handoff._render([base], texts=False).encode())
        body = base["body"] + "x" * available
        if len(body.encode()) > 2048:
            summary_extra = len(body.encode()) - 2048
            summary = base["summary"] + "y" * summary_extra
            body = body[:2048]
        else:
            summary = base["summary"]
        value = self.freeze(source=source, body=body, summary=summary)
        self.assertEqual(len(handoff._render([value], texts=False).encode()), 4096)
        with self.assertRaisesRegex(ValueError, "nomination_delivery_cap"):
            self.freeze(source=source, body=body, summary=summary + "z")
        self.assertEqual(handoff.enqueue(self.config, value)["outcome"], "queued")
        self.assertEqual(handoff.consume(self.config, self.event, 4095)["outcome"], "deferred")
        result = handoff.consume(self.config, self.event, 1_000_000)
        self.assertEqual(result["outcome"], "emitted", result)
        self.assertEqual(len(result["context"].encode()), 4096)
        self.assertIn(body, result["context"])

    def test_missing_selection_or_changed_policy_never_falls_back(self):
        value = self.freeze(); handoff.enqueue(self.config, value)
        for changes in ({"recording_mode": "off"}, {"memory_mode": "checkpoint"}):
            self.assertEqual(handoff.consume({**self.config, **changes}, self.event, 4096)["outcome"], "refused")
        missing = {k: v for k, v in self.config.items() if k != "frozen_selection"}
        self.assertEqual(handoff.consume(missing, self.event, 4096)["outcome"], "refused")
        with patch("store_registry.native_select", side_effect=OSError("owner unavailable")):
            self.assertEqual(handoff.consume(self.config, self.event, 4096)["outcome"], "unavailable")
        self.assertEqual(json.loads(self.path().read_text())["entries"][0]["status"], "pending")

    def test_lifecycle_race_and_atomic_failure_do_not_claim_delivery(self):
        value = self.freeze(); handoff.enqueue(self.config, value)
        with patch.object(handoff, "_foreground", side_effect=[True, False]):
            result = handoff.consume(self.config, self.event, 4096)
        self.assertEqual(result["outcome"], "deferred")
        self.assertNotIn("context", result)
        with patch.object(host_files, "atomic_write", side_effect=OSError("failed publication")):
            result = handoff.consume(self.config, self.event, 4096)
        self.assertEqual(result["outcome"], "unavailable")
        self.assertNotIn("context", result)
        self.assertEqual(handoff.consume(self.config, self.event, 4096)["outcome"], "emitted")

    def test_symlink_host_namespace_never_supplies_state(self):
        value = self.freeze()
        target = self.root / "elsewhere"; target.mkdir(mode=0o700)
        capture = self.config["state_dir"] / "capture"
        # Existing reader state created capture; move it intact and substitute.
        capture.rename(target / "capture")
        capture.symlink_to(target / "capture", target_is_directory=True)
        self.assertEqual(handoff.enqueue(self.config, value)["outcome"], "unavailable")
        self.assertFalse((target / "capture/claude_code/touchstone-nominations/state.json").exists())


class CodexCompatibilityTests(unittest.TestCase):
    def test_original_codex_identity_render_and_format_unchanged(self):
        value = handoff.validate_envelope(legacy_envelope())
        # Constants were produced using pre-F1 touchstone_handoff.py SHA256
        # 3cb1009486e6c7fae8af8bf1c76c41b6688db0843232e4434dd996c2d5043f89.
        self.assertEqual(value["id"], "1ff6137e3a6908a62f6980cdb304ad3e51c78eb02591908b84410183df2b5bbb")
        self.assertEqual(digest(encoded(value)), "9777df1a7a5dba6eea2d66d045606268e694d77b6f1f5208b6b60e942b4da1dd")
        self.assertEqual(digest(handoff._render([value]).encode()), "4bdc742fbe56cb487f57c021518aac48dd8d8c995bc134bf7846654fa6b2dfae")
        self.assertNotIn("source_host", value)
        changed = copy.deepcopy(value); changed["source_boundary_ref"]["ordinal_scope"] = "source_window"
        changed["id"] = handoff._identity(changed)
        with self.assertRaises(ValueError): handoff.validate_envelope(changed)
        changed = copy.deepcopy(value); changed["evidence"][0]["source_ref"] = changed["source_boundary_ref"]
        changed["id"] = handoff._identity(changed)
        with self.assertRaises(ValueError): handoff.validate_envelope(changed)


if __name__ == "__main__":
    unittest.main()
