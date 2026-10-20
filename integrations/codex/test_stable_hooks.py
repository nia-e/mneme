"""Disposable installed shell fixtures; no native host, owner, or provider."""
import contextlib
import io
import json
import os
from pathlib import Path
import shutil
import subprocess
import sys
import tempfile
import types
import unittest
from unittest import mock

import stable_hooks


FIXTURE = b'''import json, os, sys
print(json.dumps({"argv": sys.argv, "stdin": sys.stdin.buffer.read().hex(),
                  "cwd": os.getcwd(), "env": os.environ.get("MNEME_STABLE_FIXTURE")}))
print("runtime stderr", file=sys.stderr)
raise SystemExit(int(os.environ.get("MNEME_STABLE_EXIT", "0")))
'''


@unittest.skipUnless(os.name == "posix", "stable POSIX hook installation")
class StableHookTests(unittest.TestCase):
    def setUp(self):
        temporary = tempfile.TemporaryDirectory()
        self.addCleanup(temporary.cleanup)
        self.base = Path(temporary.name).resolve()
        self.root = self.base / "stable café's $(literal) ; root"
        self.program = self.base / "v1 bundle" / "hook_launcher.py"
        self.program.parent.mkdir()
        self.program.write_bytes(FIXTURE)
        self.config = self.base / "stable config's.json"
        self.config.write_bytes(b'{"fixture": true}\n')
        self.runtime = self.root / "runtimes" / "v1 executable"
        self.cwd = self.base / "work"
        self.cwd.mkdir()
        self.plan = stable_hooks.prepare(
            stable_root=self.root, runtime=self.runtime, python=sys.executable,
            hooks_program=self.program, hook_config_path=self.config)

    def publish(self, plan=None):
        plan = self.plan if plan is None else plan
        stable_hooks.install(plan)
        stable_hooks.select(plan)

    def invoke(self, *args, payload=b"", exit_code=0):
        return subprocess.run(
            stable_hooks.entrypoint_argv(self.root / "launch") + list(args),
            input=payload, cwd=self.cwd,
            env={**os.environ, "MNEME_STABLE_FIXTURE": "unchanged",
                 "MNEME_STABLE_EXIT": str(exit_code), "PATH": "/does-not-exist"},
            capture_output=True, timeout=5)

    def assert_refused(self, result):
        self.assertEqual(result.returncode, 1)
        self.assertEqual(result.stdout, b"")
        self.assertEqual(result.stderr, (stable_hooks.NOTICE + "\n").encode())

    def next_plan(self):
        program = self.base / "v2 bundle" / "hook_launcher.py"
        program.parent.mkdir()
        program.write_bytes(FIXTURE)
        return stable_hooks.prepare(
            stable_root=self.root, runtime=self.root / "runtimes" / "v2 executable",
            python=sys.executable, hooks_program=program, hook_config_path=self.config)

    def test_prepare_is_read_only_and_command_does_not_resolve_or_require_install(self):
        self.assertFalse(self.root.exists())
        self.assertEqual(stable_hooks.entrypoint_argv(self.root / "launch"),
                         ["/bin/sh", str(self.root / "launch")])
        self.assertEqual(stable_hooks.PLAN_SCHEMA, self.plan["schema"])
        link = self.base / "logical"
        link.symlink_to(self.root, target_is_directory=True)
        self.assertEqual(stable_hooks.entrypoint_argv(link / "launch")[1], str(link / "launch"))
        self.assertFalse(self.root.exists())

    def test_prepare_resolves_interpreter_and_dispatcher_but_keeps_config_spelling(self):
        interpreter_alias = self.base / "python-current"
        interpreter_alias.symlink_to(sys.executable)
        bundle_alias = self.base / "bundle-current"
        bundle_alias.symlink_to(self.program.parent, target_is_directory=True)
        plan = stable_hooks.prepare(
            stable_root=self.root, runtime=self.runtime, python=interpreter_alias,
            hooks_program=bundle_alias / "hook_launcher.py", hook_config_path=self.config)
        self.assertEqual(plan["python"], str(Path(sys.executable).resolve()))
        self.assertEqual(plan["hooks_program"], str(self.program))
        self.assertEqual(plan["hook_config_path"], str(self.config))
        interpreter_alias.unlink()
        interpreter_alias.symlink_to("/not/a/python")
        bundle_alias.unlink()
        bundle_alias.symlink_to("/not/a/bundle", target_is_directory=True)
        self.publish(plan)
        self.assertEqual(self.invoke().returncode, 0)

    def test_replacing_pinned_machinery_with_symlink_refuses_install_and_select(self):
        other = self.program.with_name("other.py")
        original = self.program.read_bytes()
        other.write_bytes(original)
        self.program.unlink()
        self.program.symlink_to(other)
        with self.assertRaisesRegex(ValueError, "machinery path changed"):
            stable_hooks.install(self.plan)
        self.assertFalse(self.root.exists())
        self.program.unlink()
        self.program.write_bytes(original)
        self.publish()
        self.program.unlink()
        self.program.symlink_to(other)
        with self.assertRaisesRegex(ValueError, "machinery path changed"):
            stable_hooks.select(self.plan)
        self.assertEqual(os.readlink(self.root / "current"), str(self.runtime))

    def test_stage_does_not_activate_and_real_artifacts_are_private(self):
        result = stable_hooks.install(self.plan)
        self.assertEqual(result["status"], "staged")
        self.assertIsNone(result["previous_runtime"])
        self.assertFalse((self.root / "current").exists())
        for path in (self.root / "launch", self.runtime):
            self.assertTrue(path.is_file())
            self.assertFalse(path.is_symlink())
            self.assertEqual(path.stat().st_mode & 0o777, 0o700)
        self.assert_refused(self.invoke())
        result = stable_hooks.select(self.plan)
        self.assertEqual(result["status"], "selected")
        self.assertTrue((self.root / "current").is_symlink())
        self.assertEqual(os.readlink(self.root / "current"), str(self.runtime))

    def test_exact_argv_binary_input_output_exit_environment_and_cwd(self):
        self.publish()
        args = ["--core", "", 'literal "quote"', "apostrophe's", "$(not executed)",
                "trailing\\", "line one\nline two", "%PATH% ! & ;"]
        payload = b"\x00\xffopaque input\n" + b"event" * 10000
        result = self.invoke(*args, payload=payload, exit_code=23)
        self.assertEqual(result.returncode, 23)
        self.assertEqual(result.stderr, b"runtime stderr\n")
        self.assertEqual(json.loads(result.stdout), {
            "argv": [str(self.program), "--config", str(self.config), *args],
            "stdin": payload.hex(), "cwd": str(self.cwd), "env": "unchanged"})

    def test_upgrade_and_rollback_keep_recipe_shim_and_old_artifacts(self):
        self.publish()
        entrypoint = self.root / "launch"
        before = entrypoint.read_bytes(), entrypoint.stat().st_ino
        recipe = stable_hooks.entrypoint_argv(entrypoint)
        second = self.next_plan()
        result = stable_hooks.install(second)
        self.assertEqual(result["previous_runtime"], str(self.runtime))
        self.assertEqual(json.loads(self.invoke().stdout)["argv"][0], str(self.program))
        result = stable_hooks.select(second)
        self.assertEqual(result["previous_runtime"], str(self.runtime))
        self.assertEqual(json.loads(self.invoke().stdout)["argv"][0], second["hooks_program"])
        self.assertEqual(before, (entrypoint.read_bytes(), entrypoint.stat().st_ino))
        self.assertEqual(recipe, stable_hooks.entrypoint_argv(entrypoint))
        result = stable_hooks.select(self.plan)
        self.assertEqual(result["previous_runtime"], second["runtime"])
        self.assertEqual(json.loads(self.invoke().stdout)["argv"][0], str(self.program))
        self.assertTrue(Path(second["runtime"]).is_file())
        self.assertTrue(self.runtime.is_file())

    def test_runtime_implementation_may_change_without_changing_recipe(self):
        self.publish()
        before = (self.root / "launch").read_bytes()
        self.program.write_bytes(b"import sys\nprint('replacement implementation')\n")
        result = self.invoke()
        self.assertEqual(result.returncode, 0)
        self.assertEqual(result.stdout, b"replacement implementation\n")
        self.assertEqual((self.root / "launch").read_bytes(), before)

    def test_started_invocation_finishes_old_runtime_after_selection_switch(self):
        self.program.write_bytes(b"import sys\nprint('old ready', flush=True)\n"
                                 b"sys.stdin.buffer.read()\nprint('old finished')\n")
        self.publish()
        second = self.next_plan()
        stable_hooks.install(second)
        child = subprocess.Popen(stable_hooks.entrypoint_argv(self.root / "launch"),
                                 stdin=subprocess.PIPE, stdout=subprocess.PIPE, stderr=subprocess.PIPE)
        try:
            self.assertEqual(child.stdout.readline(), b"old ready\n")
            stable_hooks.select(second)
            output, error = child.communicate(b"finish", timeout=5)
            self.assertEqual((child.returncode, output, error), (0, b"old finished\n", b""))
            self.assertEqual(json.loads(self.invoke().stdout)["argv"][0], second["hooks_program"])
            self.assertTrue(self.program.is_file())
        finally:
            if child.poll() is None:
                child.kill()
                child.communicate(timeout=5)

    def test_selected_runtime_has_no_updater_or_source_import_dependency(self):
        copied = self.base / "copied helper"
        copied.mkdir()
        for name in ("stable_hooks.py", "host_files.py"):
            shutil.copyfile(Path(stable_hooks.__file__).parent / name, copied / name)
        plan_file = self.base / "plan.json"
        plan_file.write_text(json.dumps(self.plan))
        for operation in ("install", "select"):
            result = subprocess.run(
                [sys.executable, "-B", str(copied / "stable_hooks.py"), operation, "--plan", str(plan_file)],
                capture_output=True, timeout=5)
            self.assertEqual(result.returncode, 0, result.stdout + result.stderr)
            self.assertEqual(json.loads(result.stdout)["operation"], operation)
        shutil.rmtree(copied)
        self.assertEqual(self.invoke().returncode, 0)

    def test_missing_regular_directory_broken_and_unexecutable_selectors_refuse(self):
        self.publish()
        selector = self.root / "current"
        selector.unlink()
        for raw in (b"", b"unrelated config\n", b"x" * 100000):
            with self.subTest(raw=raw[:70]):
                selector.write_bytes(raw)
                self.assert_refused(self.invoke(payload=b"must not be executed"))
                selector.unlink()
        selector.mkdir()
        self.assert_refused(self.invoke())
        selector.rmdir()
        selector.symlink_to("/missing/runtime")
        self.assert_refused(self.invoke())
        selector.unlink()
        selector.symlink_to(self.runtime)
        self.runtime.chmod(0o600)
        self.assert_refused(self.invoke())
        self.runtime.chmod(0o700)
        selector.unlink()
        self.assert_refused(self.invoke())

    def test_symlink_runtime_is_refused_by_plan_selection(self):
        self.publish()
        other_runtime = self.runtime.with_name("other-runtime")
        self.runtime.rename(other_runtime)
        self.runtime.symlink_to(other_runtime)
        with self.assertRaises((OSError, ValueError)):
            stable_hooks.select(self.plan)

    def test_stage_refuses_existing_wrapper_without_altering_active_files(self):
        self.publish()
        before = (self.root / "launch").read_bytes(), os.readlink(self.root / "current")
        with self.assertRaisesRegex(ValueError, "already exists"):
            stable_hooks.install(self.plan)
        self.assertEqual(before, ((self.root / "launch").read_bytes(), os.readlink(self.root / "current")))

    def test_stage_refuses_changed_or_symlink_shim_before_writing_wrapper(self):
        self.publish()
        second = self.next_plan()
        shim = self.root / "launch"
        before = os.readlink(self.root / "current")
        shim.write_bytes(b"unrelated local work")
        with self.assertRaisesRegex(ValueError, "entrypoint differs"):
            stable_hooks.install(second)
        self.assertFalse(Path(second["runtime"]).exists())
        self.assertEqual(os.readlink(self.root / "current"), before)
        shim.unlink()
        shim.symlink_to(self.runtime)
        with self.assertRaises((ValueError, OSError)):
            stable_hooks.install(second)
        self.assertFalse(Path(second["runtime"]).exists())

    def test_stage_failure_keeps_active_selector_and_preserves_unselected_artifact(self):
        self.publish()
        second = self.next_plan()
        before = os.readlink(self.root / "current")
        original = stable_hooks._new_file

        def failed_after_write(path, raw):
            original(path, raw)
            raise OSError("fixture failure after complete staged write")

        with mock.patch.object(stable_hooks, "_new_file", side_effect=failed_after_write):
            with self.assertRaises(OSError):
                stable_hooks.install(second)
        self.assertEqual(os.readlink(self.root / "current"), before)
        self.assertTrue(Path(second["runtime"]).is_file())
        self.assertEqual(json.loads(self.invoke().stdout)["argv"][0], str(self.program))
        with self.assertRaisesRegex(ValueError, "already exists"):
            stable_hooks.install(second)

    def test_select_refuses_partial_stage_changed_wrapper_or_missing_input(self):
        stable_hooks.install(self.plan)
        entrypoint = self.root / "launch"
        original = entrypoint.read_bytes()
        entrypoint.unlink()
        with self.assertRaises(FileNotFoundError):
            stable_hooks.select(self.plan)
        self.assertFalse((self.root / "current").exists())
        entrypoint.write_bytes(original)
        entrypoint.chmod(0o700)
        wrapper = self.runtime.read_bytes()
        self.runtime.write_bytes(wrapper + b"# changed\n")
        with self.assertRaisesRegex(ValueError, "wrapper differs"):
            stable_hooks.select(self.plan)
        self.assertFalse((self.root / "current").exists())
        self.runtime.write_bytes(wrapper)
        self.config.unlink()
        with self.assertRaises(FileNotFoundError):
            stable_hooks.select(self.plan)
        self.assertFalse((self.root / "current").exists())

    def test_select_refuses_malformed_selector_and_preserves_it(self):
        self.publish()
        selector = self.root / "current"
        selector.unlink()
        for target in ("relative", "/bad\npath", "/tmp/../ambiguous", "//host/path"):
            with self.subTest(target=target):
                selector.symlink_to(target)
                with self.assertRaises(ValueError):
                    stable_hooks.select(self.plan)
                self.assertEqual(os.readlink(selector), target)
                selector.unlink()
        selector.write_bytes(b"preserve unrelated work")
        with self.assertRaisesRegex(ValueError, "owned symlink"):
            stable_hooks.select(self.plan)
        self.assertEqual(selector.read_bytes(), b"preserve unrelated work")

    def test_selector_replace_failure_preserves_old_pointer_and_cleans_stage(self):
        self.publish()
        second = self.next_plan()
        stable_hooks.install(second)
        with mock.patch.object(stable_hooks.os, "replace", side_effect=OSError("fixture replace failure")):
            with self.assertRaises(OSError):
                stable_hooks.select(second)
        self.assertEqual(os.readlink(self.root / "current"), str(self.runtime))
        self.assertEqual(list(self.root.glob(".mneme-select-*")), [])

    def test_selector_sync_failure_is_reported_without_claiming_rollback(self):
        self.publish()
        second = self.next_plan()
        stable_hooks.install(second)
        with mock.patch.object(stable_hooks.host_files, "sync_directory", side_effect=OSError("fixture sync failure")):
            with self.assertRaises(OSError):
                stable_hooks.select(second)
        self.assertEqual(os.readlink(self.root / "current"), second["runtime"])
        self.assertEqual(list(self.root.glob(".mneme-select-*")), [])
        stable_hooks.select(self.plan)
        self.assertEqual(os.readlink(self.root / "current"), str(self.runtime))

    def test_cli_distinguishes_failed_replace_from_uncertain_directory_durability(self):
        self.publish()
        second = self.next_plan()
        stable_hooks.install(second)
        plan_file = self.base / "second-plan.json"
        plan_file.write_text(json.dumps(second))
        cases = [
            (stable_hooks.os, "replace", "refused", str(self.runtime)),
            (stable_hooks.host_files, "sync_directory", "selection_uncertain", second["runtime"]),
        ]
        for owner, operation, status, target in cases:
            output = io.StringIO()
            with self.subTest(status=status), contextlib.redirect_stdout(output), \
                    mock.patch.object(owner, operation, side_effect=OSError("fixture publication failure")):
                result = stable_hooks.main(["select", "--plan", str(plan_file)])
            self.assertEqual(result, 1)
            response = json.loads(output.getvalue())
            self.assertEqual(response["status"], status)
            self.assertEqual(response["operation"], "select")
            self.assertEqual(os.readlink(self.root / "current"), target)
            if status == "selection_uncertain":
                self.assertEqual(response["selector"], str(self.root / "current"))
                self.assertEqual(response["runtime"], second["runtime"])
                self.assertEqual(response["error"],
                                 "selector replaced; durability not confirmed; inspect current before retrying")

    def test_successful_replace_never_cleans_consumed_temporary_or_masks_uncertainty(self):
        self.publish()
        second = self.next_plan()
        stable_hooks.install(second)
        with mock.patch.object(Path, "unlink", side_effect=PermissionError("directory access lost")) as unlink:
            stable_hooks.select(second)
            unlink.assert_not_called()
            with mock.patch.object(stable_hooks.host_files, "sync_directory",
                                   side_effect=PermissionError("directory access lost")):
                with self.assertRaises(stable_hooks.SelectionUncertain):
                    stable_hooks.select(self.plan)
            unlink.assert_not_called()
        self.assertEqual(os.readlink(self.root / "current"), str(self.runtime))

    def test_config_bound_matches_dispatcher_limit(self):
        self.config.write_bytes(b" " * 8001)
        with self.assertRaisesRegex(ValueError, "byte limit"):
            stable_hooks.install(self.plan)
        self.assertFalse(self.root.exists())

    def test_selector_without_entrypoint_is_refused_as_partial_install(self):
        self.root.mkdir(mode=0o700)
        selector = self.root / "current"
        selector.symlink_to(self.runtime)
        with self.assertRaisesRegex(ValueError, "without its entrypoint"):
            stable_hooks.install(self.plan)
        self.assertFalse(self.runtime.exists())

    def test_invalid_plan_and_inputs_refuse_before_any_mutation(self):
        invalid = [{}, {**self.plan, "unknown": True}, {**self.plan, "schema": "unknown"}]
        for key in ("stable_root", "runtime", "python", "hooks_program", "hook_config_path"):
            for bad in (True, 1, [], {}, None, "", "relative", "/tmp/../bad", "/tmp/new\nline",
                        "/tmp/null\x00byte", "//host/path", "/" + "a" * 4096):
                invalid.append({**self.plan, key: bad})
        for bad in invalid:
            with self.subTest(plan=bad), self.assertRaises((ValueError, TypeError)):
                stable_hooks.install(bad)
            self.assertFalse(self.root.exists())
        for key in ("runtime", "python", "hooks_program", "hook_config_path"):
            bad = {**self.plan, key: str(self.root / "launch")}
            with self.subTest(key=key), self.assertRaises(ValueError):
                stable_hooks.install(bad)
            self.assertFalse(self.root.exists())
        for reserved in ("launch", "current"):
            bad = {**self.plan, "runtime": str(self.root / reserved / "nested")}
            with self.assertRaisesRegex(ValueError, "distinct"):
                stable_hooks.install(bad)
            self.assertFalse(self.root.exists())
        bad = {**self.plan, "hooks_program": str(self.program.with_name("hooks.py"))}
        with self.assertRaises((ValueError, FileNotFoundError)):
            stable_hooks.install(bad)

    def test_json_cli_parser_bounds_duplicates_unknown_fields_and_operations(self):
        plan_file = self.base / "cli-plan.json"
        bad_plans = [
            b"{" + b" " * stable_hooks.MAX_PLAN_BYTES,
            b'{"schema":"a","schema":"b"}',
            json.dumps({**self.plan, "unknown": 1}).encode(),
            b"{", b"[]" * 20000,
        ]
        for raw in bad_plans:
            plan_file.write_bytes(raw)
            output = io.StringIO()
            with contextlib.redirect_stdout(output):
                result = stable_hooks.main(["install", "--plan", str(plan_file)])
            self.assertEqual(result, 1)
            self.assertEqual(json.loads(output.getvalue())["status"], "refused")
            self.assertFalse(self.root.exists())
        with contextlib.redirect_stderr(io.StringIO()), self.assertRaises(SystemExit) as caught:
            stable_hooks.main(["activate"])
        self.assertEqual(caught.exception.code, 2)
        self.assertFalse(self.root.exists())

    def test_json_cli_prepare_matches_api_and_does_not_install(self):
        output = io.StringIO()
        args = ["prepare"]
        for key in ("stable_root", "runtime", "python", "hooks_program", "hook_config_path"):
            args.extend(["--" + key.replace("_", "-"), self.plan[key]])
        with contextlib.redirect_stdout(output):
            self.assertEqual(stable_hooks.main(args), 0)
        self.assertEqual(json.loads(output.getvalue()), self.plan)
        self.assertFalse(self.root.exists())

    def test_aggregate_plan_bound_counts_escaped_cli_bytes_and_trailing_newline(self):
        plan = {**self.plan, "stable_root": "/fixture"}
        for prefix, key in (("r", "runtime"), ("p", "python"),
                            ("h", "hooks_program"), ("c", "hook_config_path")):
            plan[key] = "/" + prefix + '"' * 4093
        size = lambda value: len((json.dumps(value, ensure_ascii=False, indent=2) + "\n").encode())
        self.assertGreater(size(plan), stable_hooks.MAX_PLAN_BYTES)
        with self.assertRaisesRegex(ValueError, "plan exceeds byte limit"):
            stable_hooks._checked(plan)
        # Each removed quotation mark saves two encoded bytes. Admit the actual
        # pretty-printed CLI representation, not a smaller compact surrogate.
        excess = size(plan) - stable_hooks.MAX_PLAN_BYTES
        plan["hook_config_path"] = plan["hook_config_path"][:-((excess + 1) // 2)]
        checked = stable_hooks._checked(plan)
        self.assertLessEqual(size(checked), stable_hooks.MAX_PLAN_BYTES)
        self.assertGreaterEqual(size(checked), stable_hooks.MAX_PLAN_BYTES - 1)
        self.assertFalse(self.root.exists())


class UnsupportedHostTests(unittest.TestCase):
    def test_windows_fails_explicitly_without_using_direct_hook_renderer(self):
        with mock.patch.object(stable_hooks, "os", types.SimpleNamespace(name="nt")):
            with self.assertRaisesRegex(ValueError, "retain direct hooks on Windows"):
                stable_hooks.entrypoint_argv("/fixture/launch")
            with self.assertRaisesRegex(ValueError, "retain direct hooks on Windows"):
                stable_hooks.prepare(stable_root="/fixture", runtime="/runtime", python="/python",
                                     hooks_program="/hook_launcher.py", hook_config_path="/config")


if __name__ == "__main__":
    unittest.main()
