"""Passive opt-in notices: real session ledger, bounded fake owner replies."""
import copy
from concurrent.futures import ThreadPoolExecutor
import hashlib
import io
import json
from pathlib import Path
import tempfile
import time
from types import SimpleNamespace
import unittest
from unittest.mock import patch

import core_hook
import hooks
import memory
from mcp_client import McpToolError
import store_registry

OWNER = "01ARZ3NDEKTSV4RRFFQ69G5FAV"
OTHER = "01ARZ3NDEKTSV4RRFFQ69G5FAW"


def core_value(summary=None):
    value = {"nodes": [{"id": OWNER, "summary": "Identity", "summary_truncated": False,
                       "body": "Keep the core cards.", "body_truncated": False,
                       "tags": ["core"], "status": "active",
                       "provenance": {"type": "conversation", "session": "fixture"}}],
             "total": 1, "truncated": False, "nodes_truncated": False,
             "bodies_truncated": False, "body_bytes_limit": 131072}
    if summary is not None:
        value["contradiction_summary"] = summary
    return value


class ContradictionNoticeTests(unittest.TestCase):
    def setUp(self):
        temporary = tempfile.TemporaryDirectory()
        self.addCleanup(temporary.cleanup)
        self.root = Path(temporary.name).resolve()
        self.state = self.root / "state"
        self.path = self.root / "hooks.json"
        self.path.write_text("{}")
        self.config = {"_config_path": self.path, "_registry_config_sha256": hashlib.sha256(b"{}").hexdigest(),
                       "state_dir": self.state}
        self.notice_state = {"state_dir": str(self.state), "session_id": "session"}
        self.summary = {"status": "complete", "open_found": 2, "rows_scanned": 3}
        self.calls = []
        self.catalog_owner = OWNER
        self.core_error = None
        self.check_lock = True
        self.selected = self.selection()
        self.native = patch("store_registry.native_select", side_effect=lambda *_, **__: self.selected)
        self.native.start()
        self.addCleanup(self.native.stop)
        outer = self
        class Client:
            timeout = 3
            def connect(self): pass
            def close(self): pass
            def call_tool(self, name, arguments):
                outer.calls.append((name, arguments.copy()))
                if name == "databases":
                    return [{"db": "memory", "name": "memory", "db_id": outer.catalog_owner, "state": "open"}]
                if name != "core":
                    raise AssertionError("notice must share the core request")
                # A core read must never occur while holding the state lock.
                if arguments.get("include_contradictions") and outer.check_lock:
                    with hooks._locked_state(outer.state, "session", blocking=False):
                        pass
                if outer.core_error:
                    raise outer.core_error
                return core_value(outer.summary)
        self.client = patch("store_registry.NativeService.native_client", side_effect=lambda *_args, **_kwargs: Client())
        self.client.start()
        self.addCleanup(self.client.stop)

    def selection(self, *, enabled=True, owner=OWNER, name="named"):
        delivery = {} if enabled is None else {"contradiction_notice": enabled}
        row = {"name": name, "store": {"id": owner, "owner": {"db": "memory", "url": "http://127.0.0.1:1"},
               "delivery": delivery}, "eligible": {key: key in ("read", "core") for key in
                  ("read", "write", "core", "context", "recall", "record", "maintenance")}}
        return store_registry.NativeSelection({"schema": "mneme.config-selection.v1", "digest": "a" * 64,
            "cwd": str(self.root), "config": {}, "active": [row]}, str(self.root / "memory.toml"), "/tmp/mnemed")

    def read(self, notice=True):
        return core_hook._collect_registry_scope(self.selected, self.selected.core[0], time.monotonic() + 3,
            core_hook.MAX_SCOPE_BYTES, notice_state=self.notice_state if notice else None,
            shared_work={"bytes": 0, "limit": 128 * 1024})

    def collect(self, selection, timeout, **options):
        return [core_hook._collect_registry_scope(selection, store, time.monotonic() + timeout,
                core_hook.MAX_SCOPE_BYTES, **options) for store in selection.core]

    def event(self, source="startup", **changes):
        return {"hook_event_name": "SessionStart", "source": source, "session_id": "session",
                "cwd": str(self.root), **changes}

    def test_default_absent_and_false_have_no_state_io_or_request_change(self):
        for enabled in (None, False):
            self.selected = self.selection(enabled=enabled)
            with patch("registry_runtime.selection", return_value=self.selected), \
                 patch("core_hook.collect_registry_core", side_effect=self.collect) as collector, \
                 patch("hooks._with_state", side_effect=AssertionError("unexpected state I/O")):
                result = core_hook.handle_hook_event(self.event(), self.config)
            self.assertNotIn("notice_state", collector.call_args.kwargs)
            self.assertNotIn("contradictions", result["hookSpecificOutput"]["additionalContext"])
            self.assertEqual(self.calls[-1], ("core", {"db": "memory", "max_body_bytes": 8192, "expected_db_id": OWNER}))
            self.assertFalse(self.state.exists())

    def test_native_delivery_bool_is_strict(self):
        for invalid in (0, 1, "true", None, [], {}):
            selected = self.selection()
            value = copy.deepcopy(selected.snapshot)
            value["active"][0]["store"]["delivery"]["contradiction_notice"] = invalid
            with self.subTest(invalid=invalid), self.assertRaises(ValueError):
                store_registry.NativeSelection(value, selected.registry_path, selected.native_cli)

    def test_nonzero_once_across_startup_resume_compact_clear_and_context_reset(self):
        with patch("registry_runtime.selection", return_value=self.selected), \
             patch("core_hook.collect_registry_core", side_effect=self.collect), patch("hooks._reader_worker"):
            for index, source in enumerate(("startup", "resume", "compact", "clear", "startup")):
                result = core_hook.handle_hook_event(self.event(source), self.config)
                text = result["hookSpecificOutput"]["additionalContext"]
                self.assertEqual("named: 2 open contradictions. `contradictions` to inspect." in text, index == 0)
                self.assertIn("Keep the core cards.", text)
                hooks._handle_async(self.event(source), {"schema": hooks.CONFIG_SCHEMA_V13,
                    "state_dir": self.state, "memory_scope": "registry", "recall_mode": "off", "recording_mode": "off"})
        attempted = json.loads(hooks._state_path(self.state, "session").read_text())
        self.assertEqual(attempted["contradiction_notice_attempts"], [OWNER])
        self.assertEqual(attempted["context_epoch"], 5)
        self.assertEqual(sum(args.get("include_contradictions", False) for name, args in self.calls if name == "core"), 1)

    def test_concurrent_core_reads_make_one_optional_attempt(self):
        # Another thread may legitimately hold the same ledger while this
        # thread reads core; separate sequential tests prove lock release.
        self.check_lock = False
        with ThreadPoolExecutor(max_workers=8) as pool:
            sections = list(pool.map(lambda _: self.read(), range(8)))
        self.assertEqual(sum("contradiction_summary" in section for section in sections), 1)
        self.assertTrue(all(section["cards"] for section in sections))
        self.assertEqual(sum(args.get("include_contradictions", False) for name, args in self.calls if name == "core"), 1)

    def test_owner_identity_not_display_alias_and_separate_session(self):
        self.assertIn("contradiction_summary", self.read())
        self.selected = self.selection(name="renamed")
        self.assertNotIn("contradiction_summary", self.read())
        self.selected = self.selection(owner=OTHER)
        self.catalog_owner = OTHER
        self.assertIn("contradiction_summary", self.read())
        self.notice_state = {**self.notice_state, "session_id": "another-session"}
        self.assertIn("contradiction_summary", self.read())

    def test_failed_optional_attempt_is_not_retried(self):
        self.core_error = McpToolError("fixture owner refusal")
        self.assertEqual(self.read()["outcome"], "unavailable")
        self.core_error = None
        self.assertNotIn("contradiction_summary", self.read())
        self.assertEqual(sum(args.get("include_contradictions", False) for name, args in self.calls if name == "core"), 1)

    def test_wrong_actual_owner_never_claims_or_reads_core(self):
        self.catalog_owner = OTHER
        with self.assertRaises(ValueError):
            self.read()
        self.assertEqual([name for name, _ in self.calls], ["databases"])
        self.assertFalse(self.state.exists())

    def test_revocation_discards_read_and_does_not_retry_notice(self):
        original = self.selected
        with patch("store_registry.revalidate", side_effect=[original, original, ValueError("selection revoked")]):
            # TargetPolicy also validates its frozen nested selection; use a
            # stable identity proof here to control the three scope rechecks.
            with patch("target_policy.TargetPolicy.from_selected"):
                with self.assertRaisesRegex(ValueError, "selection revoked"):
                    self.read()
        self.assertNotIn("contradiction_summary", self.read())
        self.assertEqual(sum(args.get("include_contradictions", False) for name, args in self.calls if name == "core"), 1)

    def test_optional_unavailable_zero_partial_and_malformed_leave_cards(self):
        for summary in ({"status": "unavailable"}, {"status": "complete", "open_found": 0, "rows_scanned": 8},
                        {"status": "partial", "open_found": 0, "rows_scanned": 256},
                        {"status": "partial", "open_found": True, "rows_scanned": 3},
                        {"status": "complete", "open_found": 4, "rows_scanned": 3},
                        {"status": "partial", "open_found": 3, "rows_scanned": 257},
                        {"status": "complete", "open_found": 2, "rows_scanned": 256},
                        {"status": "partial", "open_found": 2, "rows_scanned": 3},
                        {"status": "partial", "open_found": 0, "rows_scanned": 0},
                        {"status": "unavailable", "open_found": 2}, "garbage", float("nan"),
                        {"bad": "x" * memory.MAX_CORE_RESPONSE_BYTES}):
            with self.subTest(summary=summary):
                value = memory._core_response(core_value(summary))
                section = core_hook._pack({"scope": "named", "db": "memory"}, value, include_contradictions=True)
                self.assertEqual(section["outcome"], "ok")
                self.assertNotIn("contradiction_summary", section)
        value = core_value({"status": "partial", "open_found": 2, "rows_scanned": 256})
        section = core_hook._pack({"scope": "named", "db": "memory"}, value, include_contradictions=True)
        text = core_hook._registry_context(self.selected, [section])["hookSpecificOutput"]["additionalContext"]
        self.assertIn("named: at least 2 open contradictions.", text)
        self.assertNotIn("rows_scanned", text)

    def test_busy_malformed_and_full_notice_ledger_do_not_hide_core(self):
        for error in (hooks.HookError("hook state busy"), OSError("fixture state refused"),
                      RecursionError("fixture state nesting")):
            with self.subTest(error=error), patch("hooks.claim_contradiction_notice", side_effect=error):
                self.assertTrue(self.read()["cards"])
                self.assertNotIn("include_contradictions", self.calls[-1][1])
        self.state.mkdir(exist_ok=True, mode=0o700)
        path = hooks._state_path(self.state, "session")
        for attempted in ([True], [OWNER, OWNER], ["bad"], [OWNER] * 65):
            path.write_text(json.dumps({"schema": hooks.STATE_SCHEMA, "turns": {}, "contradiction_notice_attempts": attempted}))
            self.assertTrue(self.read()["cards"])
            self.assertNotIn("include_contradictions", self.calls[-1][1])
        owners = ["0" * 24 + alphabet[0] + alphabet[1] for alphabet in
                  (a + b for a in "01234567" for b in "01234567")]
        path.write_text(json.dumps({"schema": hooks.STATE_SCHEMA, "turns": {}, "contradiction_notice_attempts": owners}))
        self.assertTrue(self.read()["cards"])
        self.assertNotIn("include_contradictions", self.calls[-1][1])
        self.assertEqual(json.loads(path.read_text())["contradiction_notice_attempts"], owners)

    def test_child_and_missing_session_or_ledger_skip_optional_work(self):
        with patch("registry_runtime.selection", return_value=self.selected), \
             patch("core_hook.collect_registry_core", side_effect=self.collect) as collector:
            self.assertEqual(core_hook.handle_hook_event(self.event(agent_id="child"), self.config), {})
            collector.assert_not_called()
            for event, config in ((self.event(session_id=None), self.config),
                    (self.event(), {key: value for key, value in self.config.items() if key != "state_dir"})):
                text = core_hook.handle_hook_event(event, config)["hookSpecificOutput"]["additionalContext"]
                self.assertNotIn("contradictions", text)
                self.assertNotIn("notice_state", collector.call_args.kwargs)
        self.assertFalse(self.state.exists())

    def test_standalone_selection_never_gets_session_notice(self):
        with patch("core_hook.collect_registry_core", side_effect=self.collect):
            text = core_hook.handle_selection(self.event(), self.selected)["hookSpecificOutput"]["additionalContext"]
        self.assertNotIn("contradictions", text)
        self.assertFalse(self.state.exists())

    def test_notice_does_not_displace_cards_or_escape_context_envelope(self):
        target = {"scope": "n" * 128, "db": "memory"}
        value = core_value(self.summary)
        ordinary = core_hook._pack(target, value)
        limit = len(core_hook._json(ordinary).encode()) + len("partial") - len("ok")
        self.assertEqual(core_hook._pack(target, value, limit, include_contradictions=True), ordinary)
        sections = []
        remaining = core_hook.MAX_REGISTRY_OUTPUT
        for index in range(64):
            target = {"scope": "n" * 120 + str(index), "db": "memory"}
            if remaining < 256:
                break
            section = core_hook._pack(target, value, min(core_hook.MAX_SCOPE_BYTES, remaining - 1), include_contradictions=True)
            sections.append(section)
            remaining -= core_hook._section_bytes(section) + 1
        selection = SimpleNamespace(core=range(64), omissions=())
        text = core_hook._registry_context(selection, sections)["hookSpecificOutput"]["additionalContext"]
        self.assertLessEqual(len(text.encode()), core_hook.MAX_CONTEXT_BYTES)

    def test_valid_optional_metadata_yields_to_core_response_envelope(self):
        value = core_value()
        value["nodes"][0]["provenance"]["padding"] = ""
        size = len(core_hook._json(value).encode())
        value["nodes"][0]["provenance"]["padding"] = "x" * (memory.MAX_CORE_RESPONSE_BYTES - size)
        self.assertEqual(len(core_hook._json(value).encode()), memory.MAX_CORE_RESPONSE_BYTES)
        self.assertEqual(memory._core_response({**value, "contradiction_summary": self.summary}), value)
        value["nodes"][0]["provenance"]["padding"] += "x"
        with self.assertRaises(memory.BridgeError):
            memory._core_response({**value, "contradiction_summary": self.summary})

    def test_response_notice_is_charged_to_shared_native_bytes(self):
        size = len(core_hook._json(core_value(self.summary)).encode())
        shared = {"bytes": 0, "limit": 128 * 1024}
        core_hook._collect_registry_scope(self.selected, self.selected.core[0], time.monotonic() + 3,
            core_hook.MAX_SCOPE_BYTES, notice_state=self.notice_state, shared_work=shared)
        catalog = [{"db": "memory", "name": "memory", "db_id": OWNER, "state": "open"}]
        self.assertEqual(shared["bytes"], size + len(core_hook._json(catalog).encode()))

    def test_state_claim_does_not_refresh_expired_owner_deadline(self):
        clock = [0.0]
        claim = hooks.claim_contradiction_notice
        def slow_claim(*arguments):
            result = claim(*arguments)
            clock[0] = 2.0
            return result
        with patch("core_hook.time.monotonic", side_effect=lambda: clock[0]), \
             patch("hooks.claim_contradiction_notice", side_effect=slow_claim):
            result = core_hook._collect_registry_scope(self.selected, self.selected.core[0], 1.0,
                core_hook.MAX_SCOPE_BYTES, notice_state=self.notice_state)
        self.assertEqual(result["outcome"], "unavailable")
        self.assertEqual([name for name, _ in self.calls], ["databases"])
        self.assertFalse(claim(self.state, "session", OWNER))

    def test_parent_discards_malformed_optional_metadata_not_cards(self):
        section = core_hook._pack({"scope": "named", "db": "memory"}, core_value())
        section["contradiction_summary"] = {"status": "complete", "open_found": -1, "rows_scanned": 0}
        values = core_hook._read_sections((json.dumps(section) + "\n").encode(), [{"scope": "named", "db": "memory"}])
        self.assertTrue(values[0]["cards"])
        self.assertNotIn("contradiction_summary", values[0])

    def test_parent_child_wire_roundtrip_keeps_state_claim_and_notice(self):
        def subprocess(_command, input, _timeout):
            request = json.loads(input)
            self.assertEqual(request["notice_state"], self.notice_state)
            lines = []
            with patch("core_hook.sys.stdin", SimpleNamespace(buffer=io.BytesIO(input))), \
                 patch("builtins.print", side_effect=lambda line, **_: lines.append(line)):
                self.assertEqual(core_hook._registry_child_main(), 0)
            return ("\n".join(lines) + "\n").encode()
        with patch("core_hook._run_registry_collector", side_effect=subprocess):
            first = core_hook.collect_registry_core(self.selected, notice_state=self.notice_state)
            second = core_hook.collect_registry_core(self.selected, notice_state=self.notice_state)
        self.assertIn("contradiction_summary", first[0])
        self.assertNotIn("contradiction_summary", second[0])
        self.assertEqual(first[0]["cards"], second[0]["cards"])

    def test_child_rejects_malformed_session_state_before_owner_work(self):
        witness = {"schema": "mneme.native-core-selection.v1", "config_path": self.selected.registry_path,
                   "native_cli": self.selected.native_cli, "cwd": self.selected.cwd,
                   "digest": self.selected.digest, "binary_pin": self.selected.binary_pin}
        for state in ({"state_dir": "relative", "session_id": "session"},
                      {"state_dir": str(self.state), "session_id": "../escape"},
                      {"state_dir": str(self.state), "session_id": "session", "extra": True}):
            raw = json.dumps({"selection": witness, "timeout": 1, "notice_state": state}).encode()
            with self.subTest(state=state), patch("core_hook.sys.stdin", SimpleNamespace(buffer=io.BytesIO(raw))):
                self.assertEqual(core_hook._registry_child_main(), 1)
        self.assertEqual(self.calls, [])
        self.assertFalse(self.state.exists())


if __name__ == "__main__":
    unittest.main()
