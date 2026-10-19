"""Frozen child reloads; native bridge is synthetic, no owner or live state."""
import copy
import io
import json
from pathlib import Path
import shlex
import sys
import tempfile
from types import SimpleNamespace
import unittest
from unittest.mock import patch

import claude_config
import claude_hooks
import hooks
import reader_worker
import registry_runtime
import store_registry


class DiscoveryChildReplayTests(unittest.TestCase):
    def setUp(self):
        temporary = tempfile.TemporaryDirectory()
        self.addCleanup(temporary.cleanup)
        self.root = Path(temporary.name).resolve()
        self.binary = self.root / "synthetic-native"
        self.binary.write_text("#!/bin/sh\nexit 99\n")
        self.binary.chmod(0o755)
        self.path = self.root / "hooks.json"
        self.value = {"schema": registry_runtime.NATIVE_SCHEMA,
                      "state_dir": str(self.root / "state"),
                      "memory_config": str(self.root / "memory.toml"),
                      "native_cli": str(self.binary),
                      "reader_codex": str(Path(sys.executable).resolve()),
                      "reader_codex_sha256": "b" * 64}
        self.path.write_text(json.dumps(self.value))
        self.snapshot = {"schema": "mneme.config-selection.v1", "digest": "a" * 64,
                         "cwd": str(self.root), "profile": "default", "sources": [],
                         "config": {"background": {"enabled": False}},
                         "stores": [], "active": [], "read_default": None,
                         "project": {"configured": True, "root": str(self.root), "overlay": None},
                         "discovery": {"binding": {"path": str(self.root / "receipt.json"),
                                                    "sha256": "c" * 64},
                                       "boundary_sha256": "d" * 64}}

    def load(self, snapshot=None, **kwargs):
        with patch("store_registry.native_bridge", return_value=snapshot or self.snapshot):
            return hooks._config(self.path, cwd=self.root, **kwargs)

    def claude_path(self):
        path = self.root / "claude.json"
        value = claude_config.prepare(memory_config=self.root / "memory.toml", native_cli=self.binary,
            state_dir=self.root / "claude-state", reader_claude=self.binary,
            reader_model="claude-sonnet-4-20250514", librarian_effort="medium", transcripts_root=self.root,
            hooks_program=Path(claude_config.__file__).with_name("claude_hooks.py"),
            hook_config_path=path)["hook_config"]
        path.write_text(json.dumps(value))
        return path

    def launch(self, config):
        config = {**config, "recording_mode": "off", "_background_maintenance": True}
        data = {"active": None, "pending": None, "worker_pid": None, "worker_identity": None}

        def state(_config, _session, mutate, **_kwargs):
            result, _changed = mutate(data)
            return True, result

        event = {"hook_event_name": "SessionEnd", "session_id": "session"}
        with patch("reader_worker._state", side_effect=state), \
             patch("reader_worker._alive", return_value=False), \
             patch("reader_worker.host_process.spawn", return_value=SimpleNamespace(pid=987654)) as spawn, \
             patch("reader_worker.host_process.spawned_identity", return_value="synthetic"):
            outcome = reader_worker.background(config, event)
        return outcome, spawn

    def assert_replay(self, bridge):
        self.assertGreater(bridge.call_count, 0)
        for call in bridge.call_args_list:
            words = call.args[1]
            self.assertEqual(words[words.index("--expected-selection-digest") + 1], self.snapshot["digest"])
            self.assertNotIn("receipt.json", " ".join(words))

    def test_checkpoint_and_both_worker_hosts_carry_every_native_digest(self):
        local = copy.deepcopy(self.snapshot)
        local.pop("discovery")
        for snapshot in (self.snapshot, local):
            config = self.load(snapshot)
            with patch("store_registry.native_bridge", return_value=snapshot):
                claude = claude_config.load_config(self.claude_path(), cwd=self.root)
            # Constructing child argv is local shape validation, not native replay
            # while the worker-state lock is held.
            with patch("store_registry.native_bridge", side_effect=AssertionError("no bridge in argv construction")):
                words = shlex.split(hooks._checkpoint_command(config, "session", "turn"))
                self.assertEqual(words[words.index("--expected-selection-digest") + 1], self.snapshot["digest"])
                self.assertEqual(words[words.index("--selection-cwd") + 1], str(self.root))
                for host, candidate in (("codex", config), ("claude_code", claude)):
                    with self.subTest(host=host, discovery="discovery" in snapshot):
                        outcome, spawn = self.launch(candidate)
                        self.assertEqual(outcome, {"outcome": "launched"})
                        argv = spawn.call_args.args[0]
                        self.assertEqual(argv[argv.index("--expected-selection-digest") + 1], self.snapshot["digest"])
                        self.assertEqual(argv[argv.index("--selection-cwd") + 1], str(self.root))
                        self.assertNotIn("--workspace-binding", argv)
                        self.assertNotIn("receipt.json", " ".join(argv))
                        self.assertLess(len(" ".join(argv)), 1024)

    def test_legacy_nonnative_fixtures_keep_previous_child_arguments(self):
        config = self.load()
        for frozen in ({"schema": "mneme.store-selection.v1"}, {}):
            with self.subTest(frozen=frozen.get("schema")):
                candidate = {**config, "frozen_selection": frozen}
                self.assertNotIn("--expected-selection-digest",
                                 shlex.split(hooks._checkpoint_command(candidate, "session", "turn")))
                outcome, spawn = self.launch(candidate)
                self.assertEqual(outcome, {"outcome": "launched"})
                self.assertNotIn("--expected-selection-digest", spawn.call_args.args[0])

    def test_malformed_discovery_does_not_downgrade_to_fresh_child(self):
        config = self.load()
        for discovery in (None, {}, {**self.snapshot["discovery"], "boundary_sha256": "bad"}):
            with self.subTest(discovery=discovery):
                candidate = copy.deepcopy(config)
                candidate["frozen_selection"]["snapshot"]["discovery"] = discovery
                with self.assertRaises(ValueError):
                    hooks._checkpoint_command(candidate, "session", "turn")
                outcome, spawn = self.launch(candidate)
                self.assertEqual(outcome, {"outcome": "unavailable"})
                spawn.assert_not_called()

    def test_malformed_native_envelope_cannot_downgrade_to_fresh_child(self):
        config = self.load()
        frozen = config["frozen_selection"]
        missing = dict(frozen)
        missing.pop("snapshot")
        invalid = [missing, None, {**frozen, "snapshot": None}, {**frozen, "extra": True},
                   {**frozen, "schema": "unknown"}, {**frozen, "digest": "f" * 64},
                   {**frozen, "cwd": str(self.root / "other")}]
        for value in invalid:
            with self.subTest(frozen=value):
                candidate = {**config, "frozen_selection": value}
                with self.assertRaises(ValueError):
                    hooks._checkpoint_command(candidate, "session", "turn")
                outcome, spawn = self.launch(candidate)
                self.assertEqual(outcome, {"outcome": "unavailable"})
                spawn.assert_not_called()

    def test_native_load_and_followup_revalidations_use_expected_digest(self):
        local = copy.deepcopy(self.snapshot)
        local.pop("discovery")
        for snapshot in (self.snapshot, local):
            with self.subTest(discovery="discovery" in snapshot), \
                 patch("store_registry.native_bridge", return_value=snapshot) as bridge:
                config = hooks._config(self.path, cwd=self.root, expected_selection_digest=self.snapshot["digest"])
            self.assert_replay(bridge)
            # Configured but empty/defaultless is still a registry, never misc.
            self.assertEqual(config["memory_scope"], "registry")
            self.assertEqual((config["recall_mode"], config["recording_mode"]), ("off", "off"))
            self.assertNotIn("workspace_binding", config)
            self.assertEqual(config["frozen_selection"]["snapshot"]["active"], [])

    def test_child_replay_refuses_wrong_digest_before_projection(self):
        wrong_digest = {**self.snapshot, "digest": "f" * 64}
        local = copy.deepcopy(wrong_digest)
        local.pop("discovery")
        for snapshot in (wrong_digest, local):
            selected = store_registry.NativeSelection(snapshot, self.value["memory_config"], str(self.binary))
            with self.subTest(snapshot=snapshot), patch("store_registry.native_select", return_value=selected), \
                 patch("recording_jobs._config_digest", side_effect=AssertionError("no policy projection")):
                with self.assertRaises(hooks.HookError):
                    hooks._config(self.path, cwd=self.root, expected_selection_digest=self.snapshot["digest"])

    def test_claude_loader_and_worker_entrypoint_use_native_replay(self):
        path = self.claude_path()
        with patch("store_registry.native_bridge", return_value=self.snapshot) as bridge, \
             patch("hooks._config", side_effect=AssertionError("no cross-host fallback")), \
             patch("reader_worker.serve") as serve:
            self.assertEqual(reader_worker.main(["--serve", "--source-host", "claude_code", "--config", str(path),
                "--session-id", "session", "--selection-cwd", str(self.root),
                "--expected-selection-digest", self.snapshot["digest"]]), 0)
        self.assert_replay(bridge)
        serve.assert_called_once()
        self.assertEqual(serve.call_args.args[0]["source_host"], "claude_code")

    def test_codex_worker_entrypoint_uses_native_replay(self):
        with patch("store_registry.native_bridge", return_value=self.snapshot) as bridge, \
             patch("reader_worker.serve") as serve:
            self.assertEqual(reader_worker.main(["--serve", "--config", str(self.path),
                "--session-id", "session", "--selection-cwd", str(self.root),
                "--expected-selection-digest", self.snapshot["digest"]]), 0)
        self.assert_replay(bridge)
        serve.assert_called_once()

    def test_missing_or_stale_receipt_stops_child_without_fresh_retry(self):
        claude_path = self.claude_path()
        for failure in ("pinned discovery receipt is missing", "pinned discovery receipt changed"):
            for host, path in (("codex", self.path), ("claude_code", claude_path)):
                with self.subTest(failure=failure, host=host), \
                     patch("store_registry.native_bridge", side_effect=ValueError(failure)) as bridge, \
                     patch("reader_worker.serve") as serve, \
                     patch("misc_binding.choose_workspace", side_effect=AssertionError("no misc fallback")):
                    self.assertEqual(reader_worker.main(["--serve", "--source-host", host, "--config", str(path),
                        "--session-id", "session", "--selection-cwd", str(self.root),
                        "--expected-selection-digest", self.snapshot["digest"]]), 0)
                    bridge.assert_called_once()
                    self.assert_replay(bridge)
                    serve.assert_not_called()

    def test_checkpoint_replay_refusal_never_records_or_falls_back(self):
        config = self.load()
        words = shlex.split(hooks._checkpoint_command(config, "session", "turn"))
        with patch("store_registry.native_bridge", side_effect=ValueError("pinned receipt changed")) as bridge, \
             patch("hooks.checkpoint") as checkpoint, patch("sys.stderr", new_callable=io.StringIO):
            self.assertEqual(hooks.main([*words[2:], "--outcome", "none"]), 1)
        bridge.assert_called_once()
        self.assert_replay(bridge)
        checkpoint.assert_not_called()

    def test_local_only_children_do_not_fall_back_after_new_discovery_or_old_cli_refusal(self):
        local = copy.deepcopy(self.snapshot)
        local.pop("discovery")
        config = self.load(local)
        checkpoint_words = shlex.split(hooks._checkpoint_command(config, "session", "turn"))
        outcome, spawn = self.launch(config)
        self.assertEqual(outcome, {"outcome": "launched"})
        worker_words = spawn.call_args.args[0]
        for failure in ("local discovery boundary changed", "unexpected argument --expected-selection-digest"):
            with self.subTest(failure=failure), \
                 patch("store_registry.native_bridge", side_effect=ValueError(failure)) as bridge, \
                 patch("hooks.checkpoint") as checkpoint, patch("reader_worker.serve") as serve, \
                 patch("sys.stderr", new_callable=io.StringIO):
                self.assertEqual(hooks.main([*checkpoint_words[2:], "--outcome", "none"]), 1)
                self.assertEqual(reader_worker.main(worker_words[2:]), 0)
                self.assertEqual(bridge.call_count, 2)
                self.assert_replay(bridge)
                checkpoint.assert_not_called()
                serve.assert_not_called()

    def test_replay_digest_refuses_all_legacy_loaders_before_resolution(self):
        for schema in (hooks.CONFIG_SCHEMA, hooks.CONFIG_SCHEMA_V11, hooks.CONFIG_SCHEMA_V12):
            with self.subTest(schema=schema), \
                 patch("hooks._bounded_json", return_value=({"schema": schema}, b"{}")), \
                 patch("store_registry.resolve", side_effect=AssertionError("legacy resolution not permitted")):
                with self.assertRaisesRegex(hooks.HookError, "requires native"):
                    hooks._config(self.path, cwd=self.root, expected_selection_digest=self.snapshot["digest"])
        with patch("store_registry.resolve", side_effect=AssertionError("legacy resolution not permitted")):
            with self.assertRaisesRegex(ValueError, "requires native"):
                registry_runtime.load({}, self.path, b"{}", expected_selection_digest=self.snapshot["digest"])

    def test_worker_requires_workspace_for_replay(self):
        with patch("hooks._config") as load, patch("reader_worker.serve") as serve:
            self.assertEqual(reader_worker.main(["--serve", "--config", str(self.path), "--session-id", "session",
                "--expected-selection-digest", self.snapshot["digest"]]), 2)
        load.assert_not_called()
        serve.assert_not_called()

    def test_capture_initial_load_and_revalidation_share_outer_deadline(self):
        now = [100.0]

        def response(*_args, **_kwargs):
            now[0] += 1
            return self.snapshot

        with patch("store_registry.time.monotonic", side_effect=lambda: now[0]), \
             patch("store_registry.native_bridge", side_effect=response) as bridge:
            with store_registry.native_selection_budget(3):
                hooks._config(self.path, cwd=self.root)
        self.assertEqual([call.kwargs["timeout"] for call in bridge.call_args_list], [3, 2, 1])

    def test_expired_initial_load_does_not_buy_another_replay_budget(self):
        now = [100.0]

        def response(*_args, **_kwargs):
            now[0] += 4
            return self.snapshot

        with patch("store_registry.time.monotonic", side_effect=lambda: now[0]), \
             patch("store_registry.native_bridge", side_effect=response) as bridge:
            with store_registry.native_selection_budget(3), self.assertRaises(hooks.HookError):
                hooks._config(self.path, cwd=self.root)
        bridge.assert_called_once()

    def test_codex_entrypoint_budget_covers_load_and_event(self):
        for event_name, seconds in (("UserPromptSubmit", 3), ("Interrupt", 1.5), ("SessionEnd", 1.5)):
            now, observed = [100.0], []

            def load(*_args, **_kwargs):
                observed.append(store_registry.native_selection_remaining())
                now[0] += 1
                return {}

            def event(*_args, **_kwargs):
                observed.append(store_registry.native_selection_remaining())
                return {}

            with self.subTest(event=event_name), \
                 patch("store_registry.time.monotonic", side_effect=lambda: now[0]), \
                 patch("hooks._read_event", return_value={"hook_event_name": event_name, "cwd": str(self.root)}), \
                 patch("hooks._config", side_effect=load), patch("hooks.handle_event", side_effect=event), \
                 patch("sys.stdout", new_callable=io.StringIO):
                self.assertEqual(hooks.main(["--config", str(self.path)]), 0)
            self.assertEqual(observed, [seconds, seconds - 1])

    def test_claude_entrypoint_budget_covers_load_and_event(self):
        now, observed = [100.0], []

        def load(*_args, **_kwargs):
            observed.append(store_registry.native_selection_remaining())
            now[0] += 1
            return {}

        def event(*_args, **_kwargs):
            observed.append(store_registry.native_selection_remaining())
            return {}

        with patch("store_registry.time.monotonic", side_effect=lambda: now[0]), \
             patch("claude_hooks.read_event", return_value={"cwd": str(self.root)}), \
             patch("claude_config.load_config", side_effect=load), patch("claude_hooks.handle_event", side_effect=event), \
             patch("sys.stdout", new_callable=io.StringIO):
            self.assertEqual(claude_hooks.main(["--config", str(self.path)]), 0)
        self.assertEqual(observed, [3, 2])


if __name__ == "__main__":
    unittest.main()
