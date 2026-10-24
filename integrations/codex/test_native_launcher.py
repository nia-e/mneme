"""Explicit local startup authority stays outside native/imported owner metadata."""
import copy
import hashlib
import json
import os
import subprocess
import sys
from pathlib import Path
import tempfile
import unittest
from unittest.mock import Mock, patch

import launcher
from service import MAX_CONFIG_BYTES, _expected_catalog, ensure_ready, load_config, load_config_snapshot, start

ID = "01ARZ3NDEKTSV4RRFFQ69G5FAV"
OTHER = "01ARZ3NDEKTSV4RRFFQ69G5FAW"


class NativeLauncherTests(unittest.TestCase):
    def setUp(self):
        self.temp = tempfile.TemporaryDirectory()
        self.addCleanup(self.temp.cleanup)
        self.root = Path(self.temp.name).resolve()
        self.cwd = self.root
        cwd = patch("launcher.Path.cwd", side_effect=lambda: self.cwd)
        cwd.start(); self.addCleanup(cwd.stop)
        self.service_path = self.root / "service.json"
        self.memory = self.root / "memory.toml"
        self.database = self.root / "memory.db"
        self.database.write_bytes(b"not opened by launcher tests")
        self.data = dict(binary="mneme-mcp", database_name="project", database_path=str(self.database),
                         database_id=ID, port=18765, state_dir=str(self.root / "state"), working_directory=str(self.root))
        self.write_service()
        self.owner = dict(url="http://127.0.0.1:18765/", db="project", path=str(self.database))
        eligible = dict.fromkeys(("read", "write", "core", "context", "recall", "record", "maintenance"), True)
        self.snapshot = dict(schema="mneme.config-selection.v1", digest="a" * 64, config={}, cwd=str(self.root),
                             project=dict(configured=True, root=str(self.root)),
                             active=[dict(name="project", store=dict(id=ID, owner=self.owner), eligible=eligible)])

    def write_service(self):
        self.service_path.write_text(json.dumps(self.data))

    def run_launch(self, *, ensure=None, snapshots=None):
        snapshots = iter(snapshots) if snapshots is not None else None
        def bridge(*args, **kwargs):
            return copy.deepcopy(next(snapshots) if snapshots is not None else self.snapshot)
        with patch("host_platform.resolve_command", side_effect=lambda value, **kw: "/bin/" + value), \
                patch("store_registry.native_bridge", side_effect=bridge) as selector, \
                patch("launcher.ensure_ready", side_effect=ensure) as ready, \
                patch("launcher.os.execv") as execute:
            launcher.launch_native_configured(self.service_path, self.memory, self.root, ID)
        return selector, ready, execute

    def assert_refused(self, text):
        with patch("host_platform.resolve_command", side_effect=lambda value, **kw: "/bin/" + value), \
                patch("store_registry.native_bridge", return_value=self.snapshot), \
                patch("launcher.ensure_ready") as ready, patch("launcher.os.execv") as execute:
            with self.assertRaisesRegex(ValueError, text):
                launcher.launch_native_configured(self.service_path, self.memory, self.root, ID)
        ready.assert_not_called(); execute.assert_not_called()

    def test_full_router_exec_preserves_scope_without_protocol_read(self):
        self.snapshot["active"][0]["eligible"]["core"] = False
        other = copy.deepcopy(self.snapshot["active"][0])
        other["name"], other["store"]["id"] = "global", OTHER
        self.snapshot["active"].append(other)
        selector, ready, execute = self.run_launch()
        self.assertEqual(len(selector.call_args_list), 3)
        self.assertNotIn("--offline", selector.call_args_list[0].args[1])
        self.assertNotIn("--expected-selection-digest", selector.call_args_list[0].args[1])
        for call in selector.call_args_list[1:]:
            self.assertIn("--expected-selection-digest", call.args[1])
        self.assertLessEqual(ready.call_args.kwargs["lock_timeout"], 2)
        execute.assert_called_once_with("/bin/mneme-mcp", ["/bin/mneme-mcp", "--memory-config",
                                         str(self.memory), "--cwd", str(self.root)])

    def test_ordinary_subdirectory_uses_actual_cwd_for_policy_and_full_router(self):
        self.cwd = self.root / "src"
        self.cwd.mkdir()
        self.snapshot["cwd"] = str(self.cwd)
        selector, ready, execute = self.run_launch()
        for call in selector.call_args_list:
            arguments = call.args[1]
            self.assertEqual(arguments[arguments.index("--cwd") + 1], str(self.cwd))
        ready.assert_called_once()
        self.assertEqual(execute.call_args.args[1][-2:], ["--cwd", str(self.cwd)])

    def test_inherited_parent_entry_respects_actual_nested_project_boundary_before_startup(self):
        for index, (marker, content) in enumerate((("profile.json", '{"schema":"mneme.profile.v1","mode":"private"}'),
                                                 ("profile.json", '{"schema":"mneme.profile.v1","mode":"isolated"}'),
                                                 ("memory.toml", 'schema = "mneme.config.v1"\n'))):
            with self.subTest(marker=marker, content=content):
                child = self.root / f"child-{index}"
                (child / ".mneme").mkdir(parents=True)
                (child / ".mneme" / marker).write_text(content)
                self.cwd = child
                def native_policy(binary, arguments, **kwargs):
                    requested = arguments[arguments.index("--cwd") + 1]
                    snapshot = copy.deepcopy(self.snapshot)
                    snapshot["cwd"] = requested
                    if requested == str(child):
                        snapshot["project"]["root"] = str(child)
                        snapshot["active"] = []
                    return snapshot
                with patch("host_platform.resolve_command", side_effect=lambda value, **kw: "/bin/" + value), \
                        patch("store_registry.native_bridge", side_effect=native_policy) as selector, \
                        patch("launcher.ensure_ready") as ready, patch("launcher.os.execv") as execute:
                    with self.assertRaisesRegex(ValueError, "exact configured project"):
                        launcher.launch_native_configured(self.service_path, self.memory, self.root, ID)
                self.assertEqual(selector.call_args.args[1][-1], str(child))
                ready.assert_not_called(); execute.assert_not_called()

    def test_denied_and_inactive_owners_never_start(self):
        self.snapshot["active"][0]["eligible"]["read"] = False
        self.assert_refused("read-denied")
        self.snapshot["active"] = []
        self.assert_refused("inactive")

    def test_each_route_or_identity_mismatch_refuses_startup(self):
        for field, replacement in (("url", "https://example.test/"), ("db", "global"),
                                   ("path", str(self.root / "different.db")),
                                   ("token_env", "DIFFERENT"), ("ssh_mcp_port", 18766)):
            with self.subTest(field=field):
                original = copy.deepcopy(self.owner)
                self.owner[field] = replacement
                self.assert_refused("service route")
                self.owner.clear(); self.owner.update(original)
        self.data["database_id"] = OTHER
        self.write_service(); self.assert_refused("identity pin")
        self.data.pop("database_id")
        self.write_service(); self.assert_refused("identity pin")

    def test_foreign_project_root_is_not_process_authority(self):
        self.snapshot["project"]["root"] = str(self.root / "child")
        self.assert_refused("exact configured project")
        self.snapshot["project"]["root"] = str(self.root)
        self.snapshot["project"]["configured"] = False
        self.assert_refused("exact configured project")

    def test_connect_only_config_never_gets_local_process_authority(self):
        self.data = dict(mode="connect", url=self.owner["url"], database_name="project", database_path=str(self.database))
        self.write_service(); self.assert_refused("managed local")

    def test_byte_change_during_readiness_refuses_exec_despite_parsed_equality(self):
        def changed(*args, **kwargs):
            self.service_path.write_text(json.dumps(self.data, indent=2))
        with self.assertRaisesRegex(ValueError, "changed during readiness"):
            self.run_launch(ensure=changed)

    def test_changed_native_policy_refuses_before_or_after_readiness(self):
        changed = copy.deepcopy(self.snapshot); changed["digest"] = "b" * 64
        for sequence in ((self.snapshot, changed), (self.snapshot, self.snapshot, changed)):
            with self.subTest(count=len(sequence)), self.assertRaisesRegex(ValueError, "changed"):
                self.run_launch(snapshots=sequence)

    def test_service_identity_validation_and_legacy_catalog(self):
        config, digest = load_config_snapshot(self.service_path)
        self.assertEqual(digest, hashlib.sha256(self.service_path.read_bytes()).hexdigest())
        catalog = [dict(db="project", name="project", state="open", configured_path=str(self.database), db_id=ID)]
        self.assertTrue(_expected_catalog(config, catalog))
        catalog[0]["db_id"] = OTHER
        self.assertFalse(_expected_catalog(config, catalog))
        self.data.pop("database_id"); self.write_service()
        self.assertTrue(_expected_catalog(load_config(self.service_path), catalog))
        for invalid in (ID.lower(), "8" + ID[1:], False, 42, "garbage"):
            with self.subTest(identity=invalid):
                self.data["database_id"] = invalid; self.write_service()
                with self.assertRaisesRegex(ValueError, "uppercase ULID"):
                    load_config(self.service_path)

    def test_wrong_live_identity_refuses_without_lock_or_spawn(self):
        config = load_config(self.service_path)
        catalog = [dict(db="project", name="project", state="open", configured_path=str(self.database), db_id=OTHER)]
        with patch("service._probe", return_value=catalog), patch("service._lock") as lock, patch("service.spawn") as spawn:
            with self.assertRaisesRegex(RuntimeError, "unexpected database catalog"):
                ensure_ready(config, lock_timeout=.01)
        lock.assert_not_called(); spawn.assert_not_called()

    def test_busy_native_startup_lock_is_bounded_and_never_spawns(self):
        config = load_config(self.service_path)
        with patch("service._probe", return_value=None), \
                patch("service.lock_file_bounded", side_effect=BlockingIOError("busy")) as lock, patch("service.spawn") as spawn:
            with self.assertRaises(BlockingIOError):
                ensure_ready(config, lock_timeout=.01)
        self.assertEqual(lock.call_args.kwargs, dict(timeout=.01)); spawn.assert_not_called()

    def test_native_option_combinations_refused_without_protocol_read(self):
        cases = (["--service-config", str(self.service_path), "--native-cli", "mnemed"],
                 ["--service-config", str(self.service_path), "--memory-config", str(self.memory)],
                 ["--store-registry", str(self.memory), "--memory-config", str(self.memory)])
        for arguments in cases:
            with self.subTest(arguments=arguments), patch("sys.stderr"), patch("launcher.run_stream") as stream:
                with self.assertRaises(SystemExit): launcher.main(arguments)
                stream.assert_not_called()

    def test_native_main_selects_new_mode_not_relay(self):
        with patch("launcher.launch_native_configured") as native, patch("launcher.run_stream") as stream:
            launcher.main(["--service-config", str(self.service_path), "--memory-config", str(self.memory),
                           "--cwd", str(self.root), "--expected-db-id", ID])
        native.assert_called_once_with(str(self.service_path), str(self.memory), str(self.root), ID, "mnemed", "mneme-mcp")
        stream.assert_not_called()

    def test_real_exec_handoff_keeps_original_protocol_input(self):
        selector = self.root / "mnemed"
        router = self.root / "mneme-mcp"
        fixture = self.root / "selection.json"
        fixture.write_text(json.dumps(self.snapshot))
        selector.write_text(f"#!{sys.executable}\nfrom pathlib import Path\nprint(Path({str(fixture)!r}).read_text())\n")
        router.write_text(f"#!{sys.executable}\nimport json,sys\nprint(json.dumps(dict(args=sys.argv[1:], stdin=sys.stdin.read())))\n")
        selector.chmod(0o700); router.chmod(0o700)
        code = ("import launcher; launcher.ensure_ready=lambda *args, **kwargs: None; "
                f"launcher.launch_native_configured({str(self.service_path)!r}, {str(self.memory)!r}, "
                f"{str(self.root)!r}, {ID!r}, {str(selector)!r}, {str(router)!r})")
        original = '{"jsonrpc":"2.0","id":1,"method":"initialize"}\n'
        for windows_handoff in (False, True):
            with self.subTest(windows_handoff=windows_handoff):
                chosen = code.replace("import launcher;", f"import launcher; launcher.IS_WINDOWS={windows_handoff};")
                result = subprocess.run([sys.executable, "-c", chosen], input=original, text=True,
                                        capture_output=True, timeout=10, check=True, cwd=self.root,
                                        env={**os.environ, "PYTHONPATH": str(Path(__file__).parent)})
                self.assertEqual(json.loads(result.stdout), dict(args=["--memory-config", str(self.memory),
                                                                       "--cwd", str(self.root)], stdin=original))

    def test_windows_handoff_waits_and_propagates_exit_status_without_redirecting_io(self):
        with patch("host_platform.resolve_command", side_effect=lambda value, **kw: "/bin/" + value), \
                patch("store_registry.native_bridge", side_effect=lambda *a, **kw: copy.deepcopy(self.snapshot)), \
                patch("launcher.ensure_ready"), patch("launcher.IS_WINDOWS", True), \
                patch("subprocess.run", return_value=Mock(returncode=7)) as wait, patch("launcher.os.execv") as execute:
            with self.assertRaises(SystemExit) as stopped:
                launcher.launch_native_configured(self.service_path, self.memory, self.root, ID)
        self.assertEqual(stopped.exception.code, 7)
        wait.assert_called_once_with(["/bin/mneme-mcp", "--memory-config", str(self.memory),
                                      "--cwd", str(self.root)], shell=False)
        execute.assert_not_called()

    def test_wrong_identity_after_spawn_is_cleaned_up_not_published(self):
        config = load_config(self.service_path)
        process = Mock(pid=123)
        process.poll.return_value = None
        wrong = [dict(db="project", name="project", state="open", configured_path=str(self.database), db_id=OTHER)]
        with patch("service.ServiceConfig.for_launch", return_value=config), \
                patch("service._probe", side_effect=[None, wrong]), patch("service.spawn", return_value=process), \
                patch("service._terminate_spawned") as cleanup, patch("service.atomic_write") as publish:
            with self.assertRaisesRegex(RuntimeError, "unexpected database catalog"):
                start(config, timeout=.1, lock_timeout=.01)
        cleanup.assert_called_once_with(process); publish.assert_not_called()

    def test_same_image_service_reader_keeps_duplicate_and_byte_bounds(self):
        self.service_path.write_text('{"port":1,"port":2}')
        with self.assertRaisesRegex(ValueError, "duplicate"): load_config_snapshot(self.service_path)
        self.service_path.write_bytes(b" " * (MAX_CONFIG_BYTES + 1))
        with self.assertRaisesRegex(ValueError, "exceeds"): load_config_snapshot(self.service_path)


if __name__ == "__main__": unittest.main()
