"""Native hook wire adapter and durable admission; no providers or live owners."""
import copy
import hashlib
import json
from pathlib import Path
import sys
import tempfile
import unittest
from unittest.mock import MagicMock, patch

import hooks
import registry_runtime
import store_registry as registry
from target_policy import TargetPolicy, policy_for

DB = "01M3CPEVDTM9PP4HJTMMBZ8HR9"
OTHER = "01M3CV5EZP4HWAK56YJCMZQG2Y"


def snapshot(cwd, *, background=True, core=True, read=True, write=True, recall=True, record=True, maintenance=False, guidance=None):
    eligible = {"read": read, "write": write, "core": read and core,
                "context": read, "recall": background and read and recall,
                "record": background and write and record, "maintenance": background and read and write and maintenance}
    entry = {"name": "named", "store": {"id": DB, "owner": {"url": "http://127.0.0.1:1", "db": "project", "path": None},
             "guidance": guidance or {}, "capture": {"kinds": ["lesson"], "evidence_kinds": ["user_statement"],
             "required_tags": ["collaboration-preference"], "allow_associations": False,
             "max_summary_bytes": 384, "max_body_bytes": 1024}}, "eligible": eligible}
    value = {"schema": "mneme.config-selection.v1", "digest": "a" * 64,
             "cwd": str(cwd), "profile": "default", "sources": [],
             "config": {"background": {"enabled": background, "model": "gpt-6.1-sol", "effort": "medium"}},
             "stores": [{**entry, "active": True}], "active": [entry],
             "project": {"configured": False, "root": None, "overlay": None}, "read_default": "named"}
    return value


class NativeHookConfigTests(unittest.TestCase):
    def setUp(self):
        self.temp = tempfile.TemporaryDirectory()
        self.root = Path(self.temp.name).resolve()
        self.path = self.root / "hooks.json"
        self.value = {"schema": registry_runtime.NATIVE_SCHEMA, "state_dir": str(self.root / "state"),
                      "memory_config": str(self.root / "memory.toml"), "native_cli": str(self.root / "mnemed"),
                      "reader_codex": str(Path(sys.executable).resolve()), "reader_codex_sha256": "b" * 64}
        self.write()
        self.raw_snapshot = snapshot(self.root)
        self.selected = registry.NativeSelection(self.raw_snapshot, self.value["memory_config"], self.value["native_cli"], str(self.root))
        self.select_patch = patch("store_registry.native_select", side_effect=lambda *_: self.selected)
        self.select_patch.start()

    def tearDown(self):
        self.select_patch.stop()
        self.temp.cleanup()

    def write(self):
        self.path.write_text(json.dumps(self.value))

    def load(self):
        return hooks._config(self.path, cwd=str(self.root))

    def test_native_metadata_is_only_authoritative_policy(self):
        config = self.load()
        selected = registry_runtime.selection(config)
        self.assertEqual(config["frozen_selection"]["snapshot"], self.raw_snapshot)
        self.assertEqual(selected.digest, self.raw_snapshot["digest"])
        rule = selected.named("named").record
        self.assertEqual((rule.kinds, rule.evidence_kinds, rule.required_tags, rule.allow_associations,
                          rule.max_summary_bytes, rule.max_body_bytes),
                         (("lesson",), ("user_statement",), ("collaboration-preference",), False, 384, 1024))

    def test_background_off_keeps_core_and_manual_grants(self):
        self.selected = registry.NativeSelection(snapshot(self.root, background=False), self.value["memory_config"], self.value["native_cli"])
        config = self.load()
        self.assertEqual((config["recall_mode"], config["recording_mode"]), ("off", "off"))
        self.assertEqual([store.name for store in self.selected.core], ["named"])
        self.assertEqual([store.name for store in self.selected.readable], ["named"])
        self.assertTrue(self.selected.snapshot["active"][0]["eligible"]["write"])
        event = {"hook_event_name": "UserPromptSubmit", "cwd": str(self.root), "session_id": "session-1",
                 "turn_id": "turn-1", "prompt": "A substantive request that should not start automatic work"}
        with patch("hooks._reader_worker") as worker:
            self.assertEqual(hooks.handle_event(event, config), {})
            worker.assert_not_called()

    def test_independent_background_read_record_selection(self):
        for recall, record in ((False, False), (True, False), (False, True), (True, True)):
            with self.subTest(recall=recall, record=record):
                self.selected = registry.NativeSelection(snapshot(self.root, recall=recall, record=record), self.value["memory_config"], self.value["native_cli"])
                config = self.load()
                self.assertEqual(config["recall_mode"] == "automatic", recall)
                self.assertEqual(config["recording_mode"] == "automatic", record)
                self.assertEqual(bool(self.selected.recallable), recall)
                self.assertEqual(bool(self.selected.recordable), record)
                self.assertTrue(self.selected.core)

    def test_worker_argv_preserves_native_cwd_without_legacy_binding(self):
        import reader_worker
        from types import SimpleNamespace
        for recall, record, maintenance in ((True, False, False), (False, True, False), (False, False, True)):
            with self.subTest(recall=recall, record=record, maintenance=maintenance):
                self.selected = registry.NativeSelection(snapshot(self.root, recall=recall, record=record, maintenance=maintenance),
                                                        self.value["memory_config"], self.value["native_cli"])
                config = self.load()
                config["state_dir"] = self.root / f"state-{recall}-{record}-{maintenance}"
                event = {"hook_event_name": "UserPromptSubmit", "cwd": str(self.root), "session_id": "session-1",
                         "turn_id": "turn-1", "prompt": "Substantive prompt for independently selected native background roles"}
                outcome = reader_worker.notice(config, event, True)
                self.assertEqual(outcome["outcome"], "queued" if recall else "skipped")
                with patch("recording_jobs.has_work", return_value=record), \
                     patch("reader_worker.host_process.spawn", return_value=SimpleNamespace(pid=987654)) as spawn, \
                     patch("reader_worker.host_process.spawned_identity", return_value="synthetic-process"):
                    self.assertEqual(reader_worker.background(config, event)["outcome"], "launched")
                argv = spawn.call_args.args[0]
                self.assertNotIn("--workspace-binding", argv)
                self.assertEqual(argv[argv.index("--selection-cwd")+1], str(self.root))
                self.assertEqual(config["_background_maintenance"], maintenance)

    def test_known_optional_owner_failure_keeps_later_task_recall(self):
        import registry_reader
        from mcp_client import McpProtocolError
        from librarian_policy import LibrarianBudget
        Path(self.value["memory_config"]).write_text("schema='mneme.config.v1'\n")
        value = copy.deepcopy(self.raw_snapshot)
        value["active"] = [copy.deepcopy(value["active"][0]) for _ in range(2)]
        for row, name, owner in zip(value["active"], ("optionaldown", "healthy"), (DB, OTHER)):
            row["name"], row["store"]["id"] = name, owner
        self.selected = registry.NativeSelection(value, self.value["memory_config"], self.value["native_cli"])
        budget = LibrarianBudget()
        for mode in ("known", "unknown", "over_budget"):
            with self.subTest(mode=mode):
                config = self.load()
                calls, failed_cost = [], []
                def bridge(binary, argv, request, **kwargs):
                    name = argv[argv.index("--store")+1]
                    native_limit = int(argv[argv.index("--max-native-bytes")+1])
                    calls.append((name, request["tool"], native_limit + kwargs["limit"]))
                    if name == "optionaldown" and mode == "unknown":
                        raise McpProtocolError("failed response has unknown accounting")
                    envelope = {"schema": "mneme.config-call.v1", "selection_digest": self.selected.digest,
                                "store": name, "work": {"decoded_bytes": 11}}
                    if name == "optionaldown":
                        if mode == "over_budget": envelope["work"]["decoded_bytes"] = budget.native_read_bytes
                        envelope.update(ok=False, error={"kind": "native", "message": "owner unavailable"})
                        failed_cost.append(envelope["work"]["decoded_bytes"] + len(json.dumps(envelope, ensure_ascii=False).encode()))
                        return envelope
                    result = ([{"db": "project", "name": "project", "state": "open", "db_id": OTHER}]
                              if request["tool"] == "databases" else
                              {"schema": "mneme.context.v6", "core": [], "primary": [], "expansions": [], "episodes": [],
                               "observation": {"schema": 1, "learning": "disabled", "cards": []}})
                    return {**envelope, "ok": True, "result": result}
                runtime = MagicMock()
                with patch("store_registry.native_bridge", side_effect=bridge):
                    result = registry_reader.execute(config, "Investigate the requested design evidence", runtime,
                                                     lambda: True, set())
                runtime.select.assert_not_called()
                coverage, work = result["read_coverage"], result["selection"]["native_work"]
                if mode == "known":
                    self.assertEqual(coverage, {"selected": 2, "attempted": 2, "unavailable": 1,
                                                "budget_omitted": 0, "complete": False})
                    self.assertTrue(any(name == "healthy" and tool == "recall_context" for name, tool, _ in calls))
                    healthy_room = next(room for name, tool, room in calls if name == "healthy" and tool == "databases")
                    self.assertEqual(healthy_room, budget.native_read_bytes - failed_cost[0])
                    self.assertIsInstance(work["decoded_bytes"], int)
                    self.assertFalse(work["read_allowance_exhausted"])
                else:
                    self.assertFalse(any(name == "healthy" for name, _, _ in calls))
                    self.assertEqual(coverage["budget_omitted"], 1)
                    self.assertTrue(work["read_allowance_exhausted"])
                    self.assertIsNone(work["decoded_bytes"]) if mode == "unknown" else self.assertGreaterEqual(work["decoded_bytes"], budget.native_read_bytes)

    def test_multiowner_identity_matches_exact_selected_row(self):
        store = self.selected.named("named")
        policy = TargetPolicy.from_selected(store, self.selected)
        selected = {"db": "project", "name": "project", "state": "open", "db_id": DB}
        other = {"db": "user", "name": "user", "state": "open", "db_id": OTHER}
        self.assertEqual(policy.catalog_identity([other, selected]), DB)
        for rows in ([other], [selected, selected], [other, {**selected, "db_id": OTHER}],
                     [other, {**selected, "name": "wrong"}], [other, {**selected, "state": "closed"}]):
            with self.assertRaises(ValueError): policy.catalog_identity(rows)
        _, service = policy_for(Path(store.service_config), self.root, policy)
        self.assertEqual(service.database_name, "project")
        self.assertEqual(service.native_client(operation="core").operation, "core")

    def test_snapshot_change_refuses_without_rebinding_session_ledger(self):
        config = self.load()
        event = {"cwd": str(self.root), "session_id": "session-1"}
        registry_runtime.admit_session(config, event)
        original = hooks._state_path(self.root / "state", "session-1").read_bytes()
        changed = copy.deepcopy(self.raw_snapshot)
        changed["digest"] = "c" * 64
        self.selected = registry.NativeSelection(changed, self.value["memory_config"], self.value["native_cli"])
        with self.assertRaises(ValueError): registry_runtime.selection(config)
        self.assertEqual(hooks._state_path(self.root / "state", "session-1").read_bytes(), original)

    def test_nearest_machine_adapter_keeps_old_ledger_and_reader_pin(self):
        outer = {"root": str(self.root), "state_dir": str(self.root / "old-state"),
                 "reader_codex": str(Path(sys.executable).resolve()), "reader_codex_sha256": "d" * 64}
        self.value["workspace_adapters"] = [outer]
        self.write()
        config = self.load()
        self.assertEqual(config["state_dir"], self.root / "old-state")
        self.assertEqual(config["reader_codex_sha256"], "d" * 64)
        self.assertEqual(config["frozen_selection"]["digest"], self.raw_snapshot["digest"])

    def test_actual_project_keeps_existing_background_instruction_file(self):
        import registry_recording
        from recording_jobs import project_focus_witness
        value = copy.deepcopy(self.raw_snapshot)
        value["project"] = {"configured": True, "root": str(self.root), "overlay": None}
        value["active"][0]["store"]["active"] = {"workspace": "project", "roots": [str(self.root)]}
        self.selected = registry.NativeSelection(value, self.value["memory_config"], self.value["native_cli"])
        store = self.selected.named("named")
        self.assertEqual(store.workspace_root, str(self.root))
        directory = self.root / ".mneme"
        directory.mkdir()
        file = directory / "hippocampus.md"
        original = "# Capture instructions\n\nKeep scoped facts and do not claim cached reads are live.\n"
        file.write_text(original)
        focus, witness = project_focus_witness(store.workspace_root)
        acquisition = registry_recording.Acquisition(self.selected, (store,), [], 0, 10, {}, {},
                                                     {store.name: {"text": focus, **witness}})
        offered = acquisition.scope_options["selected_stores"][0]
        self.assertEqual(offered["project_focus"], focus)
        _, _, frozen = registry_recording.freeze_destination(acquisition, store.name, seconds=10)
        self.assertEqual(frozen["project_focus_witness"], witness)
        self.assertEqual(file.read_text(), original)
        # An ordinary global connection is not made a project instruction role
        # merely because it is active inside the project boundary.
        value["active"][0]["store"]["active"]["workspace"] = "any"
        any_store = registry.NativeSelection(value, self.value["memory_config"], self.value["native_cli"]).named("named")
        self.assertIsNone(any_store.workspace_root)

    def test_startup_core_does_not_fetch_or_render_guidance(self):
        import core_hook
        import time
        value = copy.deepcopy(self.raw_snapshot)
        value["active"][0]["store"]["guidance"] = {"tag_guide": {"id": OTHER, "content": "summary"}}
        self.selected = registry.NativeSelection(value, self.value["memory_config"], self.value["native_cli"])
        calls = []
        class Client:
            timeout = 3
            def connect(self): pass
            def close(self): pass
            def call_tool(self, name, args):
                calls.append((name, args))
                if name == "databases":
                    return [{"db": "project", "name": "project", "db_id": DB, "state": "open"}]
                if name == "core": return {"nodes": [], "total": 0, "truncated": False}
                raise AssertionError("unexpected foreground guide read")
        with patch("store_registry.NativeService.native_client", return_value=Client()), \
             patch("memory._core_response", side_effect=lambda value, **_: value), \
             patch("guidance.collect_guidance") as guide:
            section = core_hook._collect_registry_scope(self.selected, self.selected.named("named"),
                                                        time.monotonic()+3, core_hook.MAX_SCOPE_BYTES,
                                                        shared_work={"bytes": 0, "limit": 128*1024})
        guide.assert_not_called()
        self.assertEqual([name for name, _ in calls], ["databases", "core"])
        self.assertEqual(set(section), {"scope", "db", "outcome", "cards", "total"})
        self.assertNotIn("guidance", json.dumps(core_hook._registry_context(self.selected, [section])))

    def test_generated_adapter_cannot_add_another_human_policy(self):
        for key in ("recording_mode", "read", "store_registry", "purpose", "global_preferences"):
            with self.subTest(key=key):
                with self.assertRaises(ValueError): registry_runtime.validate_native_static({**self.value, key: True})

    def test_duplicate_owner_snapshot_is_invalid(self):
        value = copy.deepcopy(self.raw_snapshot)
        value["active"].append({**value["active"][0], "name": "another"})
        with self.assertRaises(ValueError): registry.NativeSelection(value, self.value["memory_config"], self.value["native_cli"])


class NativeCoreWireTests(unittest.TestCase):
    def test_large_native_snapshot_uses_small_frozen_core_reference(self):
        import core_hook
        from types import SimpleNamespace
        value = snapshot(Path("/tmp"))
        value["config"]["padding"] = "x" * (core_hook.MAX_REGISTRY_REQUEST + 1)
        selected = registry.NativeSelection(value, "/tmp/memory.toml", "/tmp/mnemed")
        section = {"scope": "named", "db": "project", "outcome": "empty", "cards": [], "total": 0}
        with patch("core_hook.subprocess.run", return_value=SimpleNamespace(stdout=(json.dumps(section)+"\n").encode())) as run:
            self.assertEqual(core_hook.collect_registry_core(selected), [section])
        payload = run.call_args.kwargs["input"]
        self.assertLess(len(payload), core_hook.MAX_REGISTRY_REQUEST)
        witness = json.loads(payload)["selection"]
        self.assertEqual(witness["digest"], selected.digest)
        self.assertNotIn("snapshot", witness)

    def test_known_optional_owner_failure_does_not_poison_later_core(self):
        import core_hook
        import time
        from mcp_client import McpProtocolError
        value = snapshot(Path("/tmp"))
        value["active"] = [copy.deepcopy(value["active"][0]) for _ in range(3)]
        ids = [DB, "00000000000000000000000001", OTHER]
        for row, name, owner in zip(value["active"], ("first", "optionaldown", "second"), ids):
            row["name"], row["store"]["id"] = name, owner
        selected = registry.NativeSelection(value, "/tmp/memory.toml", "/tmp/mnemed")
        for mode in ("known", "unknown", "over_budget"):
            with self.subTest(mode=mode):
                clients, called = [], []
                shared = {"bytes": 0, "limit": 128*1024}
                def client_for(service, timeout=3, **kwargs):
                    client = registry.NativeClient(service.store, timeout=timeout, operation="core")
                    clients.append(client)
                    return client
                def bridge(binary, argv, request, **kwargs):
                    name = argv[argv.index("--store")+1]
                    called.append((name, request["tool"], int(argv[argv.index("--max-native-bytes")+1])))
                    if name == "optionaldown" and mode == "unknown":
                        raise McpProtocolError("native response accounting unavailable")
                    envelope = {"schema": "mneme.config-call.v1", "selection_digest": selected.digest,
                                "store": name, "work": {"decoded_bytes": 11}}
                    if name == "optionaldown":
                        if mode == "over_budget": envelope["work"]["decoded_bytes"] = shared["limit"]
                        return {**envelope, "ok": False, "error": {"kind": "native", "message": "owner unavailable"}}
                    store = selected.named(name)
                    result = ([{"db": "project", "name": "project", "state": "open", "db_id": store.db_id}]
                              if request["tool"] == "databases" else
                              {"nodes": [], "total": 0, "truncated": False, "nodes_truncated": False,
                               "bodies_truncated": False, "body_bytes_limit": 131072})
                    return {**envelope, "ok": True, "result": result}
                sections = []
                with patch("store_registry.native_select", return_value=selected), \
                     patch("store_registry.NativeService.native_client", autospec=True, side_effect=client_for), \
                     patch("store_registry.native_bridge", side_effect=bridge):
                    for store in selected.core:
                        try:
                            section = core_hook._collect_registry_scope(selected, store, time.monotonic()+3,
                                                                        core_hook.MAX_SCOPE_BYTES, shared_work=shared)
                        except ValueError:
                            section = core_hook._unavailable(core_hook._registry_target(store))
                        sections.append(section)
                if mode == "known":
                    self.assertEqual([section["outcome"] for section in sections], ["empty", "unavailable", "empty"])
                    self.assertFalse(shared.get("unknown", False))
                    self.assertEqual(shared["bytes"], sum(client.work_bytes for client in clients))
                    later = next(allowance for name, tool, allowance in called if name == "second" and tool == "databases")
                    self.assertEqual(later, shared["limit"] - sum(client.work_bytes for client in clients[:2]))
                else:
                    self.assertFalse(any(name == "second" for name, _, _ in called))
                    self.assertEqual(sections[-1]["outcome"], "unavailable")
                    self.assertTrue(shared.get("unknown") if mode == "unknown" else shared["bytes"] >= shared["limit"])

    def test_stale_compact_core_reference_makes_no_owner_contact(self):
        import core_hook
        import io
        from types import SimpleNamespace
        selected = registry.NativeSelection(snapshot(Path("/tmp")), "/tmp/memory.toml", "/tmp/mnemed")
        witness = {"schema": "mneme.native-core-selection.v1", "config_path": selected.registry_path,
                   "native_cli": selected.native_cli, "cwd": selected.cwd, "digest": "f"*64, "binary_pin": selected.binary_pin}
        request = {"selection": witness, "timeout": 2}
        with patch("core_hook.sys.stdin", SimpleNamespace(buffer=io.BytesIO(json.dumps(request).encode()))), \
             patch("store_registry.native_select", return_value=selected), \
             patch("core_hook._collect_registry_scope") as read:
            self.assertEqual(core_hook._registry_child_main(), 1)
            read.assert_not_called()


class NativeCallTests(unittest.TestCase):
    def setUp(self):
        self.selection = registry.NativeSelection(snapshot(Path("/tmp")), "/tmp/memory.toml", "/tmp/mnemed")
        self.client = self.selection.named("named").service().native_client(operation="record")

    def envelope(self, **fields):
        return {"schema": "mneme.config-call.v1", "selection_digest": self.selection.digest,
                "store": "named", "work": {"decoded_bytes": 19}, **fields}

    def test_bounded_native_budget_and_operation_are_forwarded(self):
        self.client.max_native_bytes = 1024
        response = self.envelope(ok=True, result={"answer": 1})
        with patch("store_registry.native_bridge", return_value=response) as bridge:
            self.assertEqual(self.client.call_tool("get", {"id": DB}), {"answer": 1})
        args = bridge.call_args.args[1]
        self.assertEqual(args[args.index("--operation")+1], "record")
        self.assertEqual(args[args.index("--max-native-bytes")+1], "1024")
        self.assertNotIn("limit", bridge.call_args.kwargs)
        measured = self.client.measured_size({})
        self.assertEqual(measured, 19 + len(json.dumps(response, ensure_ascii=False).encode()))
        self.assertEqual(self.client.measured_size({}), 0)

    def test_total_allowance_splits_native_and_bridge_room_before_work(self):
        self.client.max_total_bytes = 4097
        response = self.envelope(ok=True, result={"answer": "é"})
        with patch("store_registry.native_bridge", return_value=response) as bridge:
            self.assertEqual(self.client.call_tool("get", {}), response["result"])
        args = bridge.call_args.args[1]
        native_limit = int(args[args.index("--max-native-bytes")+1])
        bridge_limit = bridge.call_args.kwargs["limit"]
        self.assertEqual((native_limit, bridge_limit), (2048, 2049))
        self.assertEqual(self.client.measured_size(None), 19 + len(json.dumps(response, ensure_ascii=False).encode()))
        self.assertEqual(self.client.measured_size(None), 0)

    def test_total_allowance_respects_separate_native_and_bridge_ceilings(self):
        self.client.max_native_bytes = 1024
        self.client.max_total_bytes = 2 * registry.MAX_NATIVE_SNAPSHOT_BYTES
        with patch("store_registry.native_bridge", return_value=self.envelope(ok=True, result={})) as bridge:
            self.client.call_tool("get", {})
        args = bridge.call_args.args[1]
        self.assertEqual(args[args.index("--max-native-bytes")+1], "1024")
        self.assertEqual(bridge.call_args.kwargs["limit"], registry.MAX_NATIVE_SNAPSHOT_BYTES)

    def test_asymmetric_read_reserves_split_room_not_just_its_actual_charge(self):
        response = self.envelope(ok=True, result={"data": ""})
        response["work"]["decoded_bytes"] = 1019
        response["result"]["data"] = "x" * (489 - len(json.dumps(response, ensure_ascii=False).encode()))
        self.assertEqual(len(json.dumps(response, ensure_ascii=False).encode()), 489)
        # Even an initially unbounded caller can use this estimate to reserve
        # enough room for both halves of a later bounded final read.
        with patch("store_registry.native_bridge", return_value=response) as bridge:
            self.client.call_tool("databases", {})
            self.assertEqual(self.client.measured_size(None), 1508)
            self.assertEqual(self.client.last_read_allowance_bytes, 2038)
            self.client.max_total_bytes = self.client.last_read_allowance_bytes
            self.client.call_tool("databases", {})
        args = bridge.call_args.args[1]
        self.assertEqual(args[args.index("--max-native-bytes")+1], "1019")
        self.assertEqual(bridge.call_args.kwargs["limit"], 1019)
        self.assertEqual(self.client.measured_size(None), 1508)
        self.assertEqual(self.client.work_bytes, 3016)

    def test_native_bridge_preserves_raw_whitespace_size_for_split_reserve(self):
        value = self.envelope(ok=True, result={"answer": "é"})
        raw = (json.dumps(value, indent=16, ensure_ascii=False) + "\n").encode()
        serialized_bytes = len(json.dumps(value, ensure_ascii=False).encode())
        self.assertGreater(len(raw), serialized_bytes)
        process, pipe = MagicMock(), MagicMock()
        process.returncode = 0
        pipe.read.side_effect = [raw, b""]
        with patch("host_process.spawn", return_value=process), \
             patch("host_process.BoundedPipe", return_value=pipe), \
             patch("host_process.terminate_tree"):
            response = registry.native_bridge("/tmp/mnemed", ["call"], limit=len(raw))
        self.assertIsInstance(response, dict)
        self.assertEqual(response, value)
        self.assertEqual(json.dumps(response), json.dumps(value))
        with patch("store_registry.native_bridge", return_value=response) as bridge:
            self.client.call_tool("databases", {})
            self.assertEqual(self.client.last_read_allowance_bytes, 2 * len(raw))
            self.assertEqual(self.client.measured_size(None), 19 + serialized_bytes)
            self.client.max_total_bytes = self.client.last_read_allowance_bytes
            self.client.call_tool("databases", {})
        self.assertEqual(bridge.call_args.kwargs["limit"], len(raw))
        self.assertEqual(self.client.measured_size(None), 19 + serialized_bytes)

    def test_total_allowance_validation_precedes_native_work(self):
        for allowance in (-1, True, "1024", 1.5):
            with self.subTest(allowance=allowance), patch("store_registry.native_bridge") as bridge:
                self.client.max_total_bytes = allowance
                with self.assertRaises(ValueError) as caught:
                    self.client.call_tool("get", {})
                self.assertNotIsInstance(caught.exception, registry.NativeReadBudgetExhausted)
                bridge.assert_not_called()
        for allowance in (0, 1):
            with self.subTest(allowance=allowance), patch("store_registry.native_bridge") as bridge:
                self.client.max_total_bytes = allowance
                with self.assertRaises(registry.NativeReadBudgetExhausted) as caught:
                    self.client.call_tool("get", {})
                self.assertEqual(caught.exception.known_work_bytes, 0)
                self.assertEqual(self.client.work_bytes, 0)
                bridge.assert_not_called()

    def test_native_receive_overshoot_is_typed_and_charged_once(self):
        self.client.max_total_bytes = 4096
        response = self.envelope(ok=False, error={"kind": "native", "message": "opaque receive failure"})
        response["work"]["decoded_bytes"] = 2050
        with patch("store_registry.native_bridge", return_value=response):
            with self.assertRaises(registry.NativeReadBudgetExhausted) as caught:
                self.client.call_tool("get", {})
        charge = 2050 + len(json.dumps(response, ensure_ascii=False).encode())
        self.assertEqual(caught.exception.known_work_bytes, charge)
        self.assertEqual(self.client.work_bytes, charge)
        self.assertEqual(self.client.measured_size(None), charge)
        self.assertEqual(self.client.measured_size(None), 0)

    def test_measured_success_over_total_room_is_typed_after_exact_charge(self):
        self.client.max_total_bytes = 512
        response = self.envelope(ok=True, result=list(range(20)))
        response["work"]["decoded_bytes"] = 256
        # Compact native JSON fits its bridge half, but Python's charged
        # reserialization adds spaces. Never silently admit that full-call cost.
        self.assertLessEqual(len(json.dumps(response, separators=(",", ":")).encode()) + 1, 256)
        with patch("store_registry.native_bridge", return_value=response):
            with self.assertRaises(registry.NativeReadBudgetExhausted) as caught:
                self.client.call_tool("get", {})
        charge = 256 + len(json.dumps(response, ensure_ascii=False).encode())
        self.assertEqual(caught.exception.known_work_bytes, charge)
        self.assertEqual(self.client.measured_size(None), charge)
        self.assertEqual(self.client.measured_size(None), 0)

    def test_budget_text_or_exact_native_limit_does_not_reclassify_failure(self):
        from mcp_client import McpProtocolError, McpToolError, McpTransportError
        self.client.max_total_bytes = 4096
        for kind, native_bytes, error_type in (("native", 2048, McpProtocolError),
                                              ("native", 19, McpProtocolError),
                                              ("input", 2050, McpProtocolError),
                                              ("protocol", 2050, McpProtocolError),
                                              ("tool", 2050, McpToolError),
                                              ("transport", 2050, McpTransportError)):
            response = self.envelope(ok=False, error={"kind": kind, "message": "shared MCP receive budget exhausted"})
            response["work"]["decoded_bytes"] = native_bytes
            with self.subTest(kind=kind, native_bytes=native_bytes), patch("store_registry.native_bridge", return_value=response):
                with self.assertRaises(error_type) as caught:
                    self.client.call_tool("get", {})
                self.assertNotIsInstance(caught.exception, registry.NativeReadBudgetExhausted)
                self.assertEqual(self.client.measured_size(None), native_bytes + len(json.dumps(response).encode()))
                self.assertEqual(self.client.measured_size(None), 0)

    def test_missing_or_untrusted_work_remains_unknown_not_budget_exhaustion(self):
        self.client.max_total_bytes = 4096
        base = self.envelope(ok=False, error={"kind": "native", "message": "receive budget exhausted"})
        base["work"]["decoded_bytes"] = 2050
        for changed in ({"work": {}}, {"work": {"decoded_bytes": None}},
                        {"work": {"decoded_bytes": True}}, {"selection_digest": "f" * 64},
                        {"store": "different"}, {"schema": "unknown"}):
            with self.subTest(changed=changed), patch("store_registry.native_bridge", return_value={**base, **changed}):
                with self.assertRaises(ValueError) as caught:
                    self.client.call_tool("get", {})
                self.assertNotIsInstance(caught.exception, registry.NativeReadBudgetExhausted)
                self.assertEqual(self.client.work_bytes, 0)
        with patch("store_registry.native_bridge", side_effect=ValueError("native config response exceeds byte limit")):
            with self.assertRaises(ValueError) as caught:
                self.client.call_tool("get", {})
            self.assertNotIsInstance(caught.exception, registry.NativeReadBudgetExhausted)
            self.assertEqual(self.client.work_bytes, 0)

    def test_uncertain_verified_save_error_keeps_native_details(self):
        from mcp_client import McpToolError
        details = {"kind": "tool", "message": "readback unavailable", "attempted": True, "accepted": True}
        with patch("store_registry.native_bridge", return_value=self.envelope(ok=False, error=details)):
            with self.assertRaises(McpToolError) as caught:
                self.client.save_verified("project", {"kind": "note"}, expected_db_id=DB)
        self.assertEqual(caught.exception.details, details)
        self.assertGreater(self.client.work_bytes, 19)

    def test_cached_read_cannot_turn_into_live_hook_evidence(self):
        result = {"id": DB, "_mneme_source": {"generation": "frozen"}}
        with patch("store_registry.native_bridge", return_value=self.envelope(ok=True, result=result)):
            with self.assertRaises(ValueError): self.client.call_tool("get", {})
        self.assertGreater(self.client.work_bytes, 19)

    def test_changed_native_executable_is_fenced_before_contact(self):
        with tempfile.TemporaryDirectory() as directory:
            binary = Path(directory).resolve() / "mnemed"
            binary.write_bytes(b"original executable witness")
            selected = registry.NativeSelection(snapshot(Path("/tmp")), "/tmp/memory.toml", str(binary),
                                                binary_pin=registry.native_binary_identity(binary))
            client = selected.named("named").service().native_client(operation="record")
            binary.write_bytes(b"replacement binary")
            with patch("store_registry.native_bridge") as bridge:
                with self.assertRaisesRegex(ValueError, "native binary changed"):
                    client.call_tool("get", {})
                bridge.assert_not_called()

    def test_unknown_or_exhausted_allowance_does_not_start_bridge(self):
        self.client.max_native_bytes = 0
        with patch("store_registry.native_bridge") as bridge:
            with self.assertRaises(ValueError): self.client.call_tool("get", {})
            bridge.assert_not_called()

    def test_wrong_selection_reply_refuses(self):
        with patch("store_registry.native_bridge", return_value={**self.envelope(ok=True, result={}), "selection_digest": "f"*64}):
            with self.assertRaises(ValueError): self.client.call_tool("get", {})


if __name__ == "__main__": unittest.main()
