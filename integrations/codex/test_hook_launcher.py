"""Real global dispatch, existing lifecycle delegation and no second owner."""
import copy
import hashlib
import io
import json
from pathlib import Path
import shutil
import shlex
import subprocess
import sys
import tempfile
import unittest
from contextlib import redirect_stderr, redirect_stdout
from types import SimpleNamespace
from unittest.mock import patch

import hook_launcher
import hooks
import install


class HookLauncherTests(unittest.TestCase):
    def setUp(self):
        temporary = tempfile.TemporaryDirectory()
        self.addCleanup(temporary.cleanup)
        self.base = Path(temporary.name).resolve()
        self.workspace = self.base / "workspace"
        self.workspace.mkdir()
        self.reader = self.base / "reader"
        self.reader.write_text("#!/bin/sh\nexit 0\n")
        self.reader.chmod(0o700)
        self.store = self.base / "misc.db"
        self.store.write_bytes(b"fixture")
        self.service = self.base / "service.json"
        self.service.write_text(json.dumps({"binary": str(self.reader), "database_name": "project",
            "database_path": str(self.store), "working_directory": str(self.base),
            "state_dir": str(self.base / "service-state"), "port": 18766}))
        self.static = {"schema": hooks.CONFIG_SCHEMA_V11, "state_dir": str(self.base / "state"),
            "service_config": str(self.service), "memory_scope": "misc", "memory_mode": "async",
            "reader_model": "gpt-6.1-sol", "librarian_effort": "medium", "recording_mode": "automatic",
            "reader_codex": str(self.reader), "reader_codex_sha256": hashlib.sha256(self.reader.read_bytes()).hexdigest(),
            "store_target": {"db_alias": "project", "database_path": str(self.store), "db_id": "1" * 26},
            "excluded_roots": []}
        self.path = self.base / "misc.json"
        self.path.write_text(json.dumps(self.static))
        self.event = {"hook_event_name": "UserPromptSubmit", "session_id": "session",
            "turn_id": "turn", "cwd": str(self.workspace), "prompt": "Investigate this design carefully"}

    def test_global_dispatch_delegates_same_lifecycle_with_explicit_bound_misc(self):
        with patch("hooks._handle_async", return_value={"same": "pipeline"}) as lifecycle:
            self.assertEqual(hook_launcher.handle_event(self.event, self.path), {"same": "pipeline"})
        config = lifecycle.call_args.args[1]
        self.assertEqual(config["memory_scope"], "misc")
        self.assertEqual(config["project_root"], self.workspace)
        self.assertEqual(config["workspace_binding"]["workspace_origin"], str(self.workspace))
        self.assertEqual(config["store_target"]["db_alias"], "project")
        self.assertNotIn("project_root", json.loads(self.path.read_text()))
        self.assertEqual(list((self.base / "state").glob("*.json")),
                         [hooks._state_path(self.base / "state", "session")])

    def test_main_uses_shared_wire_projection_before_misc_owner_selection(self):
        event = {**self.event, "hook_event_name": "PostToolUse", "transcript_path": None,
                 "tool_response": {"private_tool_result": "x" * 4_640_000}}
        output = io.StringIO()
        with patch.object(hook_launcher.sys, "stdin", SimpleNamespace(buffer=io.BytesIO(json.dumps(event).encode()))), \
                patch.object(hook_launcher, "handle_event", return_value={"delivery": "metadata"}) as handler, \
                redirect_stdout(output):
            self.assertEqual(hook_launcher.main(["--config", str(self.path)]), 0)
        self.assertEqual(json.loads(output.getvalue()), {"delivery": "metadata"})
        projected = handler.call_args.args[0]
        self.assertEqual(projected, {key: value for key, value in event.items() if key in hooks.LIFECYCLE_FIELDS})

    def test_main_ignored_child_and_denied_wire_do_not_select_or_load_owner(self):
        for raw, reason in ((json.dumps({**self.event, "agent_id": "child", "tool_response": "x" * 70_000}).encode(), None),
                            (b"{invalid private payload", "invalid_json"),
                            (b" " * (hooks.MAX_WIRE_BYTES + 1), "wire_limit")):
            for background in (False, True):
                output, errors = io.StringIO(), io.StringIO()
                args = ["--config", str(self.path)] + (["--reader-background"] if background else [])
                with patch.object(hook_launcher.sys, "stdin", SimpleNamespace(buffer=io.BytesIO(raw))), \
                        patch.object(hook_launcher, "handle_event") as handler, \
                        patch.object(hook_launcher, "choose_workspace") as choose, \
                        patch.object(hooks, "_config") as config, \
                        redirect_stdout(output), redirect_stderr(errors):
                    self.assertEqual(hook_launcher.main(args), 0)
                result = json.loads(output.getvalue())
                if reason is None or background:
                    self.assertEqual(result, {})
                else:
                    self.assertIn(f"omitted ({reason})", result["systemMessage"])
                    self.assertNotIn("unavailable", result["systemMessage"])
                handler.assert_not_called()
                choose.assert_not_called()
                config.assert_not_called()

    def test_copied_entrypoints_admit_large_body_ignore_child_and_refuse_bad_frame(self):
        runtime = self.base / "event-admission-runtime"
        runtime.mkdir()
        for name in install.PROGRAMS:
            source = Path(__file__).with_name(name)
            destination = runtime / name
            shutil.copyfile(source, destination)
            self.assertEqual(hashlib.sha256(destination.read_bytes()).digest(),
                             hashlib.sha256(source.read_bytes()).digest())
        # No SessionStart/submit: no pending reader or recording job, so no model
        # or native owner call can be launched by these PostToolUse fixtures.
        project = self.base / "reminder.json"
        project.write_text(json.dumps({"schema": hooks.CONFIG_SCHEMA,
            "project_root": str(self.workspace), "state_dir": str(self.base / "project-state")}))
        event = {**self.event, "hook_event_name": "PostToolUse", "transcript_path": None,
                 "tool_input": {}, "tool_response": "x" * 4_640_000}
        for entrypoint, config in (("hooks.py", project), ("hook_launcher.py", self.path)):
            command = [sys.executable, "-B", str(runtime / entrypoint), "--config", str(config)]
            for raw, reason in ((json.dumps(event).encode(), None),
                                (json.dumps({**event, "agent_id": "child"}).encode(), None),
                                (b"{truncated", "invalid_json"),
                                (b" " * (hooks.MAX_WIRE_BYTES + 1), "wire_limit")):
                result = subprocess.run(command, input=raw, capture_output=True, timeout=3, check=True)
                output = json.loads(result.stdout)
                if reason is None:
                    self.assertEqual(output, {})
                else:
                    self.assertIn(f"omitted ({reason})", output["systemMessage"])
                    self.assertNotIn("unavailable", output["systemMessage"])
                self.assertEqual(result.stderr, b"")

    def test_additive_global_and_project_hooks_choose_exactly_existing_project(self):
        dot = self.workspace / ".codex"
        dot.mkdir()
        dot.joinpath("hooks.json").write_text(json.dumps({"hooks": install.hook_handlers(
            "python /existing/hooks.py --config /existing/project.json", async_mode=False, recording=False)}))
        project_path = self.base / "project.json"
        project_path.write_text(json.dumps({"schema": hooks.CONFIG_SCHEMA,
            "project_root": str(self.workspace), "state_dir": str(self.base / "project-state")}))
        project = hooks._config(project_path)
        start = {**self.event, "hook_event_name": "SessionStart", "source": "startup"}
        with patch("hooks._handle_async") as misc_lifecycle:
            self.assertEqual(hook_launcher.handle_event(start, self.path), {})
            result = hooks.handle_event(start, project)
        self.assertIn("hookSpecificOutput", result)
        misc_lifecycle.assert_not_called()
        self.assertFalse((self.base / "state").exists())

    def test_exclusions_and_configured_work_do_not_even_load_owner(self):
        cases = ("excluded", "private", "isolated", "default", "empty-owner", "legacy-hooks")
        for case in cases:
            with self.subTest(case=case):
                root = self.workspace / case
                root.mkdir()
                if case == "excluded":
                    value = copy.deepcopy(self.static)
                    value["excluded_roots"] = [str(root)]
                    self.path.write_text(json.dumps(value))
                elif case == "legacy-hooks":
                    dot = root / ".codex"
                    dot.mkdir()
                    dot.joinpath("hooks.json").write_text(json.dumps({"hooks": {"Stop": [
                        {"hooks": [{"type": "command", "command": "python hooks.py --config /old.json"}]}]}}))
                else:
                    dot = root / ".mneme"
                    dot.mkdir()
                    if case != "empty-owner":
                        dot.joinpath("profile.json").write_text(json.dumps({"schema": "mneme.profile.v1", "mode": case}))
                with patch("hooks._config") as load:
                    self.assertEqual(hook_launcher.handle_event({**self.event, "cwd": str(root)}, self.path), {})
                load.assert_not_called()

    def test_unknown_configuration_refuses_before_owner_and_never_falls_back(self):
        dot = self.workspace / ".codex"
        dot.mkdir()
        dot.joinpath("hooks.json").write_text('{"unknown":true}')
        with patch("hooks._config") as load, self.assertRaises(ValueError):
            hook_launcher.handle_event(self.event, self.path)
        load.assert_not_called()

    def test_owner_unavailable_does_not_select_another_owner(self):
        with patch("hooks._config", side_effect=hooks.HookError("owner unavailable")) as load:
            with self.assertRaises(hooks.HookError):
                hook_launcher.handle_event(self.event, self.path)
        self.assertEqual(load.call_count, 1)

    def test_same_session_cannot_rebind_workspace_before_pipeline(self):
        other = self.base / "other"
        other.mkdir()
        with patch("hooks._handle_async", return_value={}) as lifecycle:
            hook_launcher.handle_event(self.event, self.path)
            result = hook_launcher.handle_event({**self.event, "cwd": str(other)}, self.path)
        self.assertEqual(lifecycle.call_count, 1)
        self.assertIn("unavailable", result["systemMessage"])

    def test_recording_off_keeps_misc_reader_and_never_calls_recorder(self):
        self.static["recording_mode"] = "off"
        self.path.write_text(json.dumps(self.static))
        with patch("hooks._reader_worker") as reader, patch("hooks._recording_jobs") as recorder:
            start = {**self.event, "hook_event_name": "SessionStart", "source": "startup"}
            result = hook_launcher.handle_event(start, self.path)
            hook_launcher.handle_event(self.event, self.path)
        self.assertIn("shared misc store", result["hookSpecificOutput"]["additionalContext"])
        reader.return_value.reset.assert_called_once()
        reader.return_value.notice.assert_called_once()
        recorder.assert_not_called()

    def test_recording_off_checkpoint_command_reloads_same_bound_device_config(self):
        self.static["recording_mode"] = "off"
        self.path.write_text(json.dumps(self.static))
        with patch("hooks._handle_async") as lifecycle:
            hook_launcher.handle_event(self.event, self.path)
        config = lifecycle.call_args.args[1]
        words = shlex.split(hooks._checkpoint_command(config, "session", "turn"))
        binding = json.loads(words[words.index("--workspace-binding") + 1])
        self.assertEqual(hooks._config(self.path, workspace_binding=binding)["workspace_binding"], config["workspace_binding"])

    def test_checkpoint_cannot_rebind_existing_misc_session(self):
        self.static["recording_mode"] = "off"
        self.path.write_text(json.dumps(self.static))
        with patch("hooks._reader_worker"):
            hook_launcher.handle_event(self.event, self.path)
        other = self.base / "other"
        other.mkdir()
        config = hooks._config(self.path, workspace_binding={"workspace_root": str(other), "workspace_origin": str(other)})
        with self.assertRaisesRegex(hooks.HookError, "workspace cannot change"):
            hooks.checkpoint(config, "session", "turn", "none", [])

    def test_global_alias_dispatch_retains_lexical_origin_and_rechecks_privacy(self):
        alias_parent = self.base / "alias-parent"
        alias_parent.mkdir()
        alias = alias_parent / "alias"
        alias.symlink_to(self.workspace, target_is_directory=True)
        event = {**self.event, "cwd": str(alias)}
        with patch("hooks._handle_async", return_value={}) as lifecycle:
            hook_launcher.handle_event(event, self.path)
        config = lifecycle.call_args.args[1]
        self.assertEqual(config["project_root"], self.workspace)
        self.assertEqual(config["workspace_binding"]["workspace_origin"], str(alias))
        marker = alias_parent / ".mneme"
        marker.mkdir()
        marker.joinpath("profile.json").write_text('{"schema":"mneme.profile.v1","mode":"private"}')
        with patch("hooks._config") as load:
            self.assertEqual(hook_launcher.handle_event(event, self.path), {})
            load.assert_not_called()

    def test_copied_dispatcher_finite_startup_and_configured_refusal(self):
        runtime = self.base / "copied-runtime"
        runtime.mkdir()
        for name in install.PROGRAMS:
            source = Path(__file__).with_name(name)
            destination = runtime / name
            shutil.copyfile(source, destination)
            self.assertEqual(hashlib.sha256(destination.read_bytes()).digest(),
                             hashlib.sha256(source.read_bytes()).digest())
        command = [sys.executable, "-B", str(runtime / "hook_launcher.py"), "--config", str(self.path)]
        start = {**self.event, "hook_event_name": "SessionStart", "source": "startup"}
        def invoke():
            result = subprocess.run(command, input=json.dumps(start), text=True, capture_output=True,
                                    timeout=8, check=True, cwd=self.workspace)
            return json.loads(result.stdout)
        self.assertIn("shared misc store", invoke()["hookSpecificOutput"]["additionalContext"])
        dot = self.workspace / ".mneme"
        dot.mkdir()
        dot.joinpath("profile.json").write_text('{"schema":"mneme.profile.v1","mode":"private"}')
        self.assertEqual(invoke(), {})

    def test_irrelevant_event_and_subagent_have_no_config_or_state_work(self):
        for extra in ({"hook_event_name": "Unknown"}, {"agent_id": "child"}, {"session_id": None},
                      {"hook_event_name": "SessionStart", "source": "unknown"}):
            with self.subTest(extra=extra), patch("hooks._bounded_json") as read:
                self.assertEqual(hook_launcher.handle_event({**self.event, **extra}, self.path), {})
                read.assert_not_called()
