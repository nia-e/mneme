from contextlib import ExitStack, redirect_stderr, redirect_stdout
import io
import json
import os
from pathlib import Path
import plistlib
import signal
import subprocess
import sys
import tempfile
import threading
import unittest
from concurrent.futures import ThreadPoolExecutor
from unittest.mock import Mock, patch

from host_files import atomic_write, ensure_private_directory
from host_platform import executable_name
from service import (ConnectConfig, RemoteConfig, client_for_service, MAX_CONFIG_BYTES, MAX_STATE_BYTES, ServiceConfig, _expected_catalog,
                     _matches_process, _state_matches_config, ensure_ready, launchd_plist,
                     load_config, main, restart, start, status, stop)


class ServiceTests(unittest.TestCase):
    def setUp(self):
        # Existing cases exercise the preserved Unix command-line/signal
        # contract. Windows identity and termination are tested separately.
        unix_contract = patch("service.IS_WINDOWS", False)
        unix_contract.start()
        self.addCleanup(unix_contract.stop)
        self.temp = tempfile.TemporaryDirectory()
        self.addCleanup(self.temp.cleanup)
        root = Path(self.temp.name).resolve()
        binary = root / "isolated" / "bin" / executable_name("mneme-mcp")
        binary.parent.mkdir(parents=True)
        binary.write_bytes(b"not run")
        binary.chmod(0o700)
        state_dir = root / "state"
        ensure_private_directory(state_dir)
        project_root = root / "project"
        project_root.mkdir()
        (project_root / "memory.db").write_bytes(b"mock store: native host not launched")
        self.config = ServiceConfig(binary, root / "project" / "memory.db", 18765,
                                    state_dir, project_root)

    def _record_state(self):
        path = self.config.state_dir / "mneme-codex-service.json"
        path.write_text(json.dumps({
            "pid": 123, "binary": str(self.config.binary),
            "project_db": str(self.config.project_db), "port": self.config.port,
            "working_directory": str(self.config.working_directory),
        }))
        return path

    def _catalog(self, **changes):
        return [{"db": "project", "name": "project", "state": "open",
                 "configured_path": str(self.config.project_db), **changes}]

    def _named_config(self, name="user", **changes):
        values = dict(binary=self.config.binary, database_name=name,
                      database_path=self.config.database_path, port=self.config.port,
                      state_dir=self.config.state_dir,
                      working_directory=self.config.working_directory)
        values.update(changes)
        return ServiceConfig(**values)

    def _config_json(self, **changes):
        data = {"binary": str(self.config.binary), "database_name": "user",
                "database_path": str(self.config.database_path), "port": self.config.port,
                "state_dir": str(self.config.state_dir),
                "working_directory": str(self.config.working_directory)}
        data.update(changes)
        path = self.config.state_dir / "config.json"
        path.write_text(json.dumps(data))
        return path

    def _mock_start(self, config):
        process = Mock(pid=123)
        process.poll.return_value = None
        with patch("service.spawn", return_value=process) as spawn, \
                patch("service._probe", side_effect=[None, self._catalog()]):
            self.assertEqual(start(config)["state"], "ready")
        state_path = config.state_dir / "mneme-codex-service.json"
        return spawn.call_args.args[0], json.loads(state_path.read_text())

    def test_path_selector_is_persisted_but_new_spawns_resolve_current_path(self):
        config = self._named_config("project", binary="mneme-mcp")
        loaded = ServiceConfig.from_json(self._config_json(
            binary="mneme-mcp", database_name="project"))
        self.assertEqual(loaded, config)
        other = self.config.binary.parent.parent / "next" / "mneme-mcp"
        other.parent.mkdir()
        other.write_bytes(b"not executed")
        other.chmod(0o700)
        for target in (self.config.binary, other):
            with patch.dict(os.environ, {"PATH": str(target.parent)}):
                argv, state = self._mock_start(config)
            self.assertEqual(argv[0], str(target.resolve()))
            self.assertEqual(state["binary"], "mneme-mcp")
            self.assertEqual(state["launch_binary"], str(target.resolve()))
            (config.state_dir / "mneme-codex-service.json").unlink()

    def test_symlink_selector_replacement_keeps_live_witness_and_updates_next_spawn(self):
        selector = self.config.binary.parent / "current"
        selector.symlink_to(self.config.binary)
        config = self._named_config("project", binary=selector)
        argv, state = self._mock_start(config)
        other = selector.parent / "replacement"
        other.write_bytes(b"not executed")
        other.chmod(0o700)
        selector.unlink()
        selector.symlink_to(other)
        self.assertEqual(config.binary, selector)
        self.assertEqual(state["binary"], str(selector))
        good_ps = subprocess.CompletedProcess([], 0, stdout=" ".join(argv) + "\n")
        with patch("service.subprocess.run", return_value=good_ps), \
                patch("service._probe", return_value=self._catalog()), \
                patch("service.spawn", side_effect=AssertionError("must reuse owner")):
            self.assertEqual(status(config)["state"], "ready")
            self.assertEqual(start(config)["state"], "ready")
        wrong_ps = subprocess.CompletedProcess([], 0, stdout=" ".join(config.for_launch().argv) + "\n")
        with patch("service.subprocess.run", return_value=wrong_ps):
            self.assertEqual(status(config)["state"], "stale")
        (config.state_dir / "mneme-codex-service.json").unlink()
        next_argv, _ = self._mock_start(config)
        self.assertEqual(next_argv[0], str(other.resolve()))

    def test_path_disappearance_does_not_strand_live_owner_or_authorize_replacement(self):
        config = self._named_config("project", binary="mneme-mcp")
        with patch.dict(os.environ, {"PATH": str(self.config.binary.parent)}):
            argv, _ = self._mock_start(config)
        good_ps = subprocess.CompletedProcess([], 0, stdout=" ".join(argv) + "\n")
        with patch.dict(os.environ, {"PATH": ""}), \
                patch("service.subprocess.run", return_value=good_ps), \
                patch("service._probe", return_value=self._catalog()), \
                patch("service.spawn", side_effect=AssertionError("must reuse owner")):
            self.assertEqual(start(config)["state"], "ready")
            self.assertTrue(ensure_ready(config)["reuse_only"])
            with patch("service._matches_process", side_effect=[True, True, False]), \
                    patch("service.os.kill") as kill:
                self.assertEqual(stop(config)["state"], "stopped")
                kill.assert_called_once_with(123, signal.SIGTERM)
        with patch.dict(os.environ, {"PATH": ""}), \
                patch("service.spawn", side_effect=AssertionError("no fallback")):
            with self.assertRaisesRegex(ValueError, "missing or not executable"):
                start(config)

    def test_selected_executable_failure_never_retries_or_publishes(self):
        config = self._named_config("project", binary="mneme-mcp")
        with patch.dict(os.environ, {"PATH": str(self.config.binary.parent)}), \
                patch("service._probe", return_value=None), \
                patch("service.spawn", side_effect=OSError("selected command failed")) as spawn:
            with self.assertRaisesRegex(OSError, "selected command failed"):
                start(config)
        spawn.assert_called_once()
        self.assertFalse((config.state_dir / "mneme-codex-service.json").exists())

    def test_named_config_and_legacy_project_constructor_compatibility(self):
        user = self._named_config()
        self.assertEqual(user.database_name, "user")
        self.assertEqual(user.database_path, self.config.project_db)
        self.assertIn("user=%s" % user.database_path, user.validate().argv)
        self.assertNotIn("project=", " ".join(user.argv))
        self.assertEqual(user.argv[1:3], ["--capability-profile", "operator"])
        self.assertNotIn("--allow-direct-feedback", user.argv)
        with self.assertRaisesRegex(ValueError, "only available for project"):
            _ = user.project_db
        legacy_keyword = ServiceConfig(binary=self.config.binary,
                                       project_db=self.config.project_db,
                                       port=self.config.port, state_dir=self.config.state_dir,
                                       working_directory=self.config.working_directory)
        self.assertEqual(legacy_keyword, self.config)
        self.assertEqual(self._named_config("project"), self.config)
        with self.assertRaisesRegex(ValueError, "cannot be mixed"):
            ServiceConfig(self.config.binary, self.config.project_db,
                          database_name="user", database_path=self.config.project_db)

    def test_canonical_json_and_legacy_project_json(self):
        for name in ("user", "project", "scratch", "archive.work-2"):
            with self.subTest(name=name):
                loaded = ServiceConfig.from_json(self._config_json(database_name=name))
                self.assertEqual(loaded, self._named_config(name))
        path = self._config_json()
        data = json.loads(path.read_text())
        data["project_db"] = data.pop("database_path")
        del data["database_name"]
        path.write_text(json.dumps(data))
        self.assertEqual(ServiceConfig.from_json(path), self.config)

    def test_json_rejects_mixed_missing_and_invalid_database_identity(self):
        path = self._config_json()
        canonical = json.loads(path.read_text())
        invalid = [dict(canonical, project_db=str(self.config.project_db))]
        for key in ("database_name", "database_path"):
            missing = dict(canonical)
            del missing[key]
            invalid.append(missing)
            invalid.append(dict(missing, project_db=str(self.config.project_db)))
        for name in ("", "a/b", "hello world", "x" * 129, "mémoire", None, True, ["user"]):
            invalid.append(dict(canonical, database_name=name))
        for database_path in ("relative.db", None, True, []):
            invalid.append(dict(canonical, database_path=database_path))
        with patch("service._probe", side_effect=AssertionError("unexpected HTTP")), \
                patch("service._lock", side_effect=AssertionError("unexpected lock")), \
                patch("service.subprocess.Popen", side_effect=AssertionError("unexpected spawn")):
            for data in invalid:
                with self.subTest(data=data):
                    path.write_text(json.dumps(data))
                    with self.assertRaises(ValueError):
                        ServiceConfig.from_json(path)

    def test_json_config_is_bounded_and_duplicate_names_are_rejected(self):
        path = self._config_json()
        encoded = path.read_text()
        path.write_text(encoded + " " * (MAX_CONFIG_BYTES - len(encoded)))
        self.assertEqual(ServiceConfig.from_json(path), self._named_config())
        with path.open("a") as config_file:
            config_file.write(" ")
        with self.assertRaisesRegex(ValueError, "exceeds"):
            ServiceConfig.from_json(path)
        path.write_text(encoded[:-1] + ', "database_name": "project"}')
        with self.assertRaisesRegex(ValueError, "duplicate"):
            ServiceConfig.from_json(path)

    def test_user_catalog_requires_exact_name_path_and_single_database(self):
        config = self._named_config()
        catalog = [{"db": "user", "name": "user", "state": "open",
                    "configured_path": str(config.database_path)}]
        self.assertTrue(_expected_catalog(config, catalog))
        invalid = [[], catalog * 2, {"databases": catalog}, self._catalog()]
        for changes in ({"db": "project"}, {"name": "project"}, {"state": "maintenance"},
                        {"configured_path": str(config.database_path) + ".other"}):
            invalid.append([{**catalog[0], **changes}])
        path = self._record_state()
        state = json.loads(path.read_text())
        state.pop("project_db")
        state.update(database_name="user", database_path=str(config.database_path))
        path.write_text(json.dumps(state))
        with patch("service._lock", side_effect=AssertionError("unexpected lock")), \
                patch("service.subprocess.Popen", side_effect=AssertionError("unexpected spawn")):
            for bad in invalid:
                with self.subTest(catalog=bad), patch("service._probe", return_value=bad):
                    self.assertFalse(_expected_catalog(config, bad))
                    with self.assertRaisesRegex(RuntimeError, "unexpected database catalog"):
                        ensure_ready(config)
            with patch("service._probe", return_value=catalog):
                self.assertTrue(ensure_ready(config)["reuse_only"])

    def test_state_identity_preserves_legacy_project_but_never_user(self):
        path = self._record_state()
        legacy = json.loads(path.read_text())
        user = self._named_config()
        self.assertTrue(_state_matches_config(self.config, legacy))
        self.assertFalse(_state_matches_config(user, legacy))
        canonical = {key: value for key, value in legacy.items() if key != "project_db"}
        canonical.update(database_name="user", database_path=str(user.database_path))
        self.assertTrue(_state_matches_config(user, canonical))
        self.assertFalse(_state_matches_config(self.config, canonical))
        invalid = [legacy, dict(canonical, database_name="project"),
                   dict(canonical, database_path=str(user.database_path) + ".other"),
                   dict(canonical, project_db=str(user.database_path))]
        for key in ("database_name", "database_path"):
            incomplete = dict(canonical)
            del incomplete[key]
            invalid.append(incomplete)
        with patch("service._probe", side_effect=AssertionError("unexpected HTTP")), \
                patch("service._lock", side_effect=AssertionError("unexpected lock")), \
                patch("service._matches_process", side_effect=AssertionError("unexpected ps")):
            for state in invalid:
                with self.subTest(state=state):
                    path.write_text(json.dumps(state))
                    self.assertEqual(status(user)["state"], "foreign-state")
                    with self.assertRaisesRegex(RuntimeError, "foreign service state"):
                        ensure_ready(user)

    def test_user_missing_store_or_invalid_identity_fails_before_resources(self):
        user = self._named_config(database_path=self.config.database_path.parent / "absent.db",
                                  state_dir=self.config.state_dir / "absent")
        invalid = self._named_config(database_name="invalid/name")
        with patch("service._probe", side_effect=AssertionError("unexpected HTTP")), \
                patch("service._lock", side_effect=AssertionError("unexpected lock")), \
                patch("service.subprocess.Popen", side_effect=AssertionError("unexpected spawn")):
            for config in (user, invalid):
                for operation in (start, ensure_ready, launchd_plist):
                    with self.subTest(config=config, operation=operation), self.assertRaises(ValueError):
                        operation(config)
        self.assertFalse(user.database_path.exists())
        self.assertFalse(user.state_dir.exists())

    def test_user_start_publishes_canonical_identity_and_checks_catalog(self):
        user = self._named_config()
        catalog = [{"db": "user", "name": "user", "state": "open",
                    "configured_path": str(user.database_path)}]
        process = Mock(pid=123)
        process.poll.return_value = None
        with patch("service._probe", side_effect=[None, catalog]), \
                patch("service.subprocess.Popen", return_value=process) as spawn:
            self.assertEqual(start(user)["state"], "ready")
        self.assertEqual(spawn.call_args.args[0], user.argv)
        path = user.state_dir / "mneme-codex-service.json"
        state = json.loads(path.read_text())
        self.assertEqual(state["database_name"], "user")
        self.assertEqual(state["database_path"], str(user.database_path))
        self.assertNotIn("project_db", state)
        self.assertTrue(_state_matches_config(user, state))
        path.unlink()
        with patch("service._probe", side_effect=[None, self._catalog()]), \
                patch("service.subprocess.Popen", return_value=process), \
                patch("service._terminate_spawned") as terminate:
            with self.assertRaisesRegex(RuntimeError, "unexpected database catalog"):
                start(user)
            terminate.assert_called_once_with(process)
        self.assertFalse(path.exists())

    def test_status_and_direct_start_require_exact_catalog_before_reuse(self):
        for config in (self.config, self._named_config()):
            path = self._record_state()
            if config.database_name == "user":
                state = json.loads(path.read_text())
                state.pop("project_db")
                state.update(database_name="user", database_path=str(config.database_path))
                path.write_text(json.dumps(state))
            original_state = path.read_bytes()
            catalog = [{"db": config.database_name, "name": config.database_name,
                        "state": "open", "configured_path": str(config.database_path)}]
            other_name = "project" if config.database_name == "user" else "user"
            invalid = [[{**catalog[0], "db": other_name}],
                       [{**catalog[0], "name": other_name}],
                       [{**catalog[0], "configured_path": str(config.database_path) + ".other"}],
                       catalog + [{"db": other_name, "name": other_name,
                                   "state": "open", "configured_path": "/other.db"}]]
            with patch("service._matches_process", return_value=True), \
                    patch("service.subprocess.Popen") as spawn, \
                    patch("service.os.kill") as kill:
                for bad in invalid:
                    with self.subTest(name=config.database_name, catalog=bad), \
                            patch("service._probe", return_value=bad):
                        self.assertEqual(status(config)["state"], "unexpected-catalog")
                        with self.assertRaisesRegex(RuntimeError, "unexpected-catalog"):
                            start(config)
                        self.assertEqual(path.read_bytes(), original_state)
                with patch("service._probe", return_value=catalog):
                    self.assertEqual(status(config)["state"], "ready")
                    self.assertEqual(start(config)["state"], "ready")
                spawn.assert_not_called()
                kill.assert_not_called()

    def test_unexpected_catalog_stop_retains_exact_process_identity_guard(self):
        path = self._record_state()
        wrong_catalog = self._catalog(name="user", db="user")
        with patch("service._probe", return_value=wrong_catalog), \
                patch("service._matches_process", side_effect=[True, None]), \
                patch("service.os.kill") as kill:
            with self.assertRaisesRegex(RuntimeError, "identity became unverified"):
                stop(self.config)
            kill.assert_not_called()
        self.assertTrue(path.exists())
        with patch("service._probe", return_value=wrong_catalog), \
                patch("service._matches_process", side_effect=[True, True, False]), \
                patch("service.os.kill") as kill:
            self.assertEqual(stop(self.config)["state"], "stopped")
            kill.assert_called_once_with(123, signal.SIGTERM)
        self.assertFalse(path.exists())

    def test_project_only_operator_command_and_plist_not_installed(self):
        args = self.config.validate().argv
        self.assertEqual(args[:3], [str(self.config.binary), "--capability-profile", "operator"])
        self.assertIn("project=%s" % self.config.project_db, args)
        self.assertNotIn("--allow-direct-feedback", args)
        self.assertNotIn("user=", " ".join(args))
        plist = plistlib.loads(launchd_plist(self.config))
        self.assertEqual(plist["ProgramArguments"], args)
        self.assertEqual(plist["Label"], "local.mneme.codex")
        self.assertEqual(plist["WorkingDirectory"], str(self.config.working_directory))

    def test_status_refuses_foreign_state_and_reports_stale(self):
        state = self.config.state_dir / "mneme-codex-service.json"
        state.write_text(json.dumps({"pid": 123, "binary": "/different",
                                     "project_db": str(self.config.project_db),
                                     "port": self.config.port,
                                     "working_directory": str(self.config.working_directory)}))
        self.assertEqual(status(self.config)["state"], "foreign-state")
        state.write_text(json.dumps({"pid": 123, "binary": str(self.config.binary),
                                     "project_db": str(self.config.project_db),
                                     "port": self.config.port,
                                     "working_directory": str(self.config.working_directory)}))
        with patch("service._matches_process", return_value=False):
            self.assertEqual(status(self.config)["state"], "stale")

    def test_denied_process_probe_never_marks_state_stale_or_signals(self):
        state_path = self.config.state_dir / "mneme-codex-service.json"
        state_path.write_text(json.dumps({
            "pid": 123, "binary": str(self.config.binary),
            "project_db": str(self.config.project_db), "port": self.config.port,
            "working_directory": str(self.config.working_directory),
        }))
        denied = subprocess.CompletedProcess(args=[], returncode=1, stdout="",
                                             stderr="operation not permitted")
        with patch("service.subprocess.run", return_value=denied), \
                patch("service.os.kill", side_effect=PermissionError) as kill, \
                patch("service.subprocess.Popen") as spawn:
            self.assertIsNone(_matches_process(self.config, 123))
            self.assertEqual(status(self.config)["state"], "identity-unverified")
            with self.assertRaisesRegex(RuntimeError, "identity-unverified"):
                start(self.config)
            with self.assertRaisesRegex(RuntimeError, "identity-unverified"):
                stop(self.config)
            spawn.assert_not_called()
            self.assertTrue(all(call.args == (123, 0) for call in kill.call_args_list))
        self.assertTrue(state_path.exists())

    def test_failed_ps_only_means_stale_when_pid_is_confirmed_absent(self):
        failed = subprocess.CompletedProcess(args=[], returncode=1, stdout="", stderr="")
        with patch("service.subprocess.run", return_value=failed), \
                patch("service.os.kill", side_effect=ProcessLookupError):
            self.assertFalse(_matches_process(self.config, 123))

    def test_config_requires_explicit_absolute_paths_and_token(self):
        bad = ServiceConfig(Path("relative/mneme-mcp"), self.config.project_db,
                            self.config.port, self.config.state_dir, self.config.working_directory)
        with self.assertRaises(ValueError):
            bad.validate()
        missing = ServiceConfig(self.config.binary, self.config.project_db,
                                self.config.port, self.config.state_dir,
                                self.config.working_directory, "UNSET_MNEME_TEST_TOKEN")
        with patch.dict(os.environ, {}, clear=True):
            with self.assertRaises(ValueError):
                missing.validate()
        with patch.dict(os.environ, {"MNEME_TEST_TOKEN": "secret"}):
            tokenized = ServiceConfig(self.config.binary, self.config.project_db,
                                      self.config.port, self.config.state_dir,
                                      self.config.working_directory, "MNEME_TEST_TOKEN")
            self.assertEqual(tokenized.argv[-2:], ["--http-token-env", "MNEME_TEST_TOKEN"])
            with self.assertRaises(ValueError):
                launchd_plist(tokenized)
        with self.assertRaises(ValueError):
            ServiceConfig(self.config.binary, self.config.project_db, True,
                          self.config.state_dir, self.config.working_directory).validate()

    def test_start_does_not_claim_an_existing_host_on_port(self):
        with patch("service._probe", return_value={"databases": []}), \
                patch("service.subprocess.Popen") as spawn:
            with self.assertRaisesRegex(RuntimeError, "already responds"):
                start(self.config)
            spawn.assert_not_called()

    def test_spawn_uses_explicit_working_directory(self):
        process = Mock(pid=123)
        process.poll.return_value = None
        with patch("service._probe", side_effect=[None, self._catalog()]), \
                patch("service.subprocess.Popen", return_value=process) as spawn:
            self.assertEqual(start(self.config)["state"], "ready")
        self.assertEqual(spawn.call_args.kwargs["cwd"], self.config.working_directory)

    def test_ensure_ready_serializes_concurrent_starters_and_reuses_host(self):
        spawned = threading.Event()
        process = Mock(pid=123)
        process.poll.return_value = None

        def spawn(*args, **kwargs):
            spawned.set()
            return process

        def probe(*args, **kwargs):
            return self._catalog() if spawned.is_set() else None

        barrier = threading.Barrier(2)
        def worker():
            barrier.wait()
            return ensure_ready(self.config)

        with patch("service._probe", side_effect=probe), \
                patch("service._matches_process", return_value=True), \
                patch("service.subprocess.Popen", side_effect=spawn) as popen, \
                ThreadPoolExecutor(max_workers=2) as pool:
            futures = [pool.submit(worker) for _ in range(2)]
            results = [future.result(timeout=3) for future in futures]
        self.assertEqual([result["state"] for result in results], ["ready", "ready"])
        self.assertEqual([result["pid"] for result in results], [123, 123])
        popen.assert_called_once()

    def test_ensure_ready_reuses_matching_host_without_lock_or_ps(self):
        self._record_state()
        with patch("service._probe", return_value=self._catalog()) as probe, \
                patch("service._lock", side_effect=AssertionError("unexpected lock")), \
                patch("service._matches_process", side_effect=AssertionError("unexpected ps")), \
                patch("service.subprocess.Popen", side_effect=AssertionError("unexpected spawn")):
            result = ensure_ready(self.config)
        self.assertEqual(result["state"], "ready")
        self.assertTrue(result["reuse_only"])
        self.assertEqual(result["process_identity"], "unchecked")
        probe.assert_called_once()

    def test_ensure_ready_waits_for_concurrent_state_publication_without_ps(self):
        first_read = threading.Event()
        from service import _read_state

        def read(config):
            state = _read_state(config)
            if state is None:
                first_read.set()
            return state

        with patch("service._read_state", side_effect=read), \
                patch("service._probe", return_value=self._catalog()), \
                patch("service._lock", side_effect=AssertionError("unexpected lock")), \
                patch("service._matches_process", side_effect=AssertionError("unexpected ps")), \
                ThreadPoolExecutor(max_workers=1) as pool:
            future = pool.submit(ensure_ready, self.config, 1.0)
            self.assertTrue(first_read.wait(timeout=1))
            self._record_state()  # The first starter publishes after binding HTTP.
            result = future.result(timeout=2)
        self.assertEqual(result["state"], "ready")
        self.assertTrue(result["reuse_only"])

    def test_ensure_ready_refuses_wrong_or_extra_database_catalog(self):
        state_path = self._record_state()
        catalogs = [self._catalog(configured_path=str(self.config.project_db) + ".other"),
                    self._catalog(state="maintenance"),
                    self._catalog() + [{"db": "user", "name": "user", "state": "open",
                                        "configured_path": "/other.db"}],
                    {"databases": self._catalog()}]
        with patch("service._lock", side_effect=AssertionError("unexpected lock")), \
                patch("service.subprocess.Popen", side_effect=AssertionError("unexpected spawn")):
            for catalog in catalogs:
                with self.subTest(catalog=catalog), patch("service._probe", return_value=catalog):
                    with self.assertRaisesRegex(RuntimeError, "unexpected database catalog"):
                        ensure_ready(self.config)
        self.assertTrue(state_path.exists())

    def test_ensure_ready_refuses_foreign_or_unverified_unreachable_state(self):
        state_path = self._record_state()
        with patch("service._probe", return_value=None), \
                patch("service._matches_process", return_value=None), \
                patch("service.subprocess.Popen") as spawn:
            with self.assertRaisesRegex(RuntimeError, "identity-unverified"):
                ensure_ready(self.config)
            spawn.assert_not_called()
        with patch("service._probe", return_value=None), \
                patch("service._matches_process", return_value=True), \
                patch("service.subprocess.Popen") as spawn:
            with self.assertRaisesRegex(RuntimeError, "unreachable"):
                ensure_ready(self.config)
            spawn.assert_not_called()
        data = json.loads(state_path.read_text())
        data["binary"] = "/different"
        state_path.write_text(json.dumps(data))
        with patch("service._probe", return_value=self._catalog()), \
                patch("service._lock", side_effect=AssertionError("unexpected lock")):
            with self.assertRaisesRegex(RuntimeError, "foreign service state"):
                ensure_ready(self.config)

    def test_ensure_ready_refuses_uninitialized_store_before_lock_or_spawn(self):
        self.config.project_db.unlink()
        with patch("service.subprocess.Popen") as popen:
            with self.assertRaisesRegex(ValueError, "initialize an explicit capture store"):
                ensure_ready(self.config)
            popen.assert_not_called()
        self.assertFalse((self.config.state_dir / "service.lock").exists())

    def test_removed_binary_and_missing_token_do_not_strand_managed_host(self):
        config = ServiceConfig(self.config.binary, self.config.project_db,
                               self.config.port, self.config.state_dir,
                               self.config.working_directory, "MNEME_TEST_TOKEN")
        config_path = self.config.state_dir / "config.json"
        config_path.write_text(json.dumps({
            "binary": str(config.binary), "project_db": str(config.project_db),
            "port": config.port, "state_dir": str(config.state_dir),
            "working_directory": str(config.working_directory),
            "token_env": config.token_env,
        }))
        state_path = self.config.state_dir / "mneme-codex-service.json"
        state_path.write_text(json.dumps({
            "pid": 123, "binary": str(config.binary),
            "project_db": str(config.project_db), "port": config.port,
            "working_directory": str(config.working_directory),
        }))
        config.binary.unlink()
        config.project_db.unlink()
        with patch.dict(os.environ, {}, clear=True), \
                patch("service._probe", return_value=None), \
                patch("service._matches_process", side_effect=[True, True, True, False]), \
                patch("service.os.kill") as kill:
            loaded = ServiceConfig.from_json(config_path)
            self.assertEqual(status(loaded)["state"], "unreachable")
            self.assertEqual(stop(loaded)["state"], "stopped")
            kill.assert_called_once()
        self.assertFalse(state_path.exists())

    def test_absent_or_directory_store_refuses_before_spawn_and_state(self):
        self.config.project_db.unlink()
        state_path = self.config.state_dir / "mneme-codex-service.json"
        with patch("service.subprocess.Popen") as spawn:
            with self.assertRaisesRegex(ValueError, "initialize an explicit capture store first"):
                start(self.config)
            spawn.assert_not_called()
        self.assertFalse(state_path.exists())
        self.assertFalse((self.config.state_dir / "service.lock").exists())
        self.assertFalse(self.config.project_db.exists())
        self.config.project_db.mkdir()
        with patch("service.subprocess.Popen") as spawn:
            with self.assertRaisesRegex(ValueError, "existing file"):
                start(self.config)
            spawn.assert_not_called()
        with self.assertRaisesRegex(ValueError, "existing file"):
            launchd_plist(self.config)

    def test_managed_stop_timeout_does_not_force_kill_or_forget_lease(self):
        state_path = self.config.state_dir / "mneme-codex-service.json"
        state_path.write_text(json.dumps({
            "pid": 123, "binary": str(self.config.binary),
            "project_db": str(self.config.project_db), "port": self.config.port,
            "working_directory": str(self.config.working_directory),
        }))
        with patch("service._probe", return_value=None), \
                patch("service._matches_process", return_value=True), \
                patch("service.os.kill") as kill:
            with self.assertRaisesRegex(RuntimeError, "did not stop"):
                stop(self.config, timeout=0)
            kill.assert_called_once_with(123, signal.SIGTERM)
        self.assertTrue(state_path.exists())

    def test_process_identity_is_exact_even_with_spaces_and_near_prefixes(self):
        binary = self.config.binary.parent / executable_name("mneme mcp")
        project_db = self.config.project_db.parent / "memory notes.db"
        config = ServiceConfig(binary, project_db, 1876, self.config.state_dir,
                               self.config.working_directory)
        intended = " ".join(config.argv)
        def ps(command):
            return subprocess.CompletedProcess(args=[], returncode=0,
                                               stdout=command + "\n", stderr="")
        with patch("service.subprocess.run", return_value=ps(intended)):
            self.assertTrue(_matches_process(config, 123))
        for other in (intended.replace("127.0.0.1:1876", "127.0.0.1:18765"),
                      intended.replace("memory notes.db", "memory notes.db.other"),
                      intended + " --unrelated"):
            with self.subTest(other=other), \
                    patch("service.subprocess.run", return_value=ps(other)):
                self.assertFalse(_matches_process(config, 123))


class ServicePortabilityTests(unittest.TestCase):
    def setUp(self):
        temp = tempfile.TemporaryDirectory()
        self.addCleanup(temp.cleanup)
        self.root = Path(temp.name).resolve()
        binary = self.root / "program space \u03bb.exe"
        binary.write_bytes(b"never executed except through mocked spawn")
        binary.chmod(0o700)
        database = self.root / "memory.db"
        database.write_bytes(b"not a real store")
        state_dir = self.root / "state"
        ensure_private_directory(state_dir)
        self.config = ServiceConfig(binary, database, 18765, state_dir, self.root)
        self.path = state_dir / "mneme-codex-service.json"
        self.identity = {"platform": "windows", "pid": 123,
                         "created": 987654, "executable": str(binary)}
        self.catalog = [{"db": "project", "name": "project", "state": "open",
                         "configured_path": str(database)}]

    def _record(self, **changes):
        state = {"pid": 123, "binary": str(self.config.binary),
                 "database_name": "project", "database_path": str(self.config.database_path),
                 "port": self.config.port, "working_directory": str(self.root),
                 "process_identity": self.identity}
        state.update(changes)
        atomic_write(self.path, json.dumps(state).encode())

    def _windows(self):
        stack = ExitStack()
        stack.enter_context(patch("service.IS_WINDOWS", True))
        for name in ("os.kill", "subprocess.run"):
            stack.enter_context(patch("service." + name,
                                      side_effect=AssertionError("Windows must not use " + name)))
        return stack

    def test_windows_start_records_spawn_handle_identity_and_portable_detach(self):
        process = Mock(pid=123)
        process.poll.return_value = None
        with self._windows(), patch("service.spawn", return_value=process) as spawn, \
                patch("service.spawned_identity", return_value=self.identity) as captured, \
                patch("service._probe", side_effect=[None, self.catalog]):
            self.assertEqual(start(self.config)["state"], "ready")
        captured.assert_called_once_with(process)
        self.assertTrue(spawn.call_args.kwargs["detached"])
        self.assertNotIn("start_new_session", spawn.call_args.kwargs)
        self.assertEqual(json.loads(self.path.read_text())["process_identity"], self.identity)

    def test_windows_logical_selector_uses_recorded_concrete_identity(self):
        config = ServiceConfig("mneme-mcp", self.config.database_path, self.config.port,
                               self.config.state_dir, self.root)
        self._record(binary="mneme-mcp", launch_binary=str(self.config.binary))
        with self._windows(), patch.dict(os.environ, {"PATH": ""}), \
                patch("service.process_identity", return_value=self.identity), \
                patch("service._probe", return_value=self.catalog), \
                patch("service.spawn", side_effect=AssertionError("must reuse owner")):
            self.assertEqual(status(config)["state"], "ready")
            self.assertEqual(start(config)["state"], "ready")
        wrong = {**self.identity, "executable": str(self.root / "replacement.exe")}
        with self._windows(), patch("service.process_identity", return_value=wrong):
            self.assertEqual(status(config)["state"], "stale")

    def test_windows_native_binary_validation_refuses_shell_wrappers_before_work(self):
        with self._windows(), patch("host_platform.WINDOWS", True), \
                patch("service._probe", side_effect=AssertionError("unexpected HTTP")), \
                patch("service.spawn", side_effect=AssertionError("unexpected spawn")):
            self.assertEqual(self.config.validate().binary, self.config.binary)
            for suffix in (".cmd", ".bat", ".ps1", ""):
                wrapper = self.root / ("mneme-mcp" + suffix)
                wrapper.write_bytes(b"not a native executable")
                wrapper.chmod(0o700)
                config = ServiceConfig(wrapper, self.config.database_path, self.config.port,
                                       self.config.state_dir, self.root)
                with self.subTest(suffix=suffix), self.assertRaisesRegex(ValueError, "not executable"):
                    start(config)
        self.assertFalse(self.path.exists())

    def test_windows_missing_spawn_identity_never_publishes(self):
        process = Mock(pid=123)
        process.poll.return_value = None
        with self._windows(), patch("service.spawn", return_value=process), \
                patch("service.spawned_identity", return_value=None), \
                patch("service._probe", return_value=None), \
                patch("service._terminate_spawned") as terminate:
            with self.assertRaisesRegex(RuntimeError, "identity unavailable"):
                start(self.config)
        terminate.assert_called_once_with(process)
        self.assertFalse(self.path.exists())

    def test_state_publication_failure_terminates_only_the_spawned_process(self):
        process = Mock(pid=123)
        process.poll.return_value = None
        with self._windows(), patch("service.spawn", return_value=process), \
                patch("service.spawned_identity", return_value=self.identity), \
                patch("service._probe", side_effect=[None, self.catalog]), \
                patch("service.atomic_write", side_effect=OSError("publication denied")), \
                patch("service._terminate_spawned") as terminate:
            with self.assertRaisesRegex(OSError, "publication denied"):
                start(self.config)
        terminate.assert_called_once_with(process)
        self.assertFalse(self.path.exists())

    def test_windows_current_creation_and_image_must_match_recorded_identity(self):
        self._record()
        for current, expected in ((self.identity, "ready"),
                                  ({**self.identity, "created": 987655}, "stale"),
                                  ({**self.identity, "executable": str(self.config.binary) + ".other"}, "stale")):
            with self.subTest(current=current), self._windows(), \
                    patch("service.process_identity", return_value=current), \
                    patch("service._probe", return_value=self.catalog):
                self.assertEqual(status(self.config)["state"], expected)

    def test_windows_unverifiable_or_legacy_identity_cannot_authorize_stop(self):
        identities = [None, {**self.identity, "created": True},
                      {**self.identity, "pid": 124},
                      {**self.identity, "executable": str(self.root / "other.exe")}]
        for identity in identities:
            self._record(process_identity=identity)
            with self.subTest(identity=identity), self._windows(), \
                    patch("service.process_alive", return_value=True), \
                    patch("service.process_identity", return_value=None), \
                    patch("service.terminate_verified") as terminate, \
                    patch("service._probe", return_value=self.catalog):
                self.assertEqual(status(self.config)["state"], "identity-unverified")
                # HTTP reuse still grants no process-control authority.
                self.assertEqual(ensure_ready(self.config)["process_identity"], "unchecked")
                with self.assertRaisesRegex(RuntimeError, "identity-unverified"):
                    stop(self.config)
                terminate.assert_not_called()
                self.assertTrue(self.path.exists())
        self._record()
        with self._windows(), patch("service.process_identity", return_value=None), \
                patch("service.process_alive", return_value=None):
            self.assertEqual(status(self.config)["state"], "identity-unverified")

    def test_windows_stop_requires_same_handle_final_verification(self):
        self._record()
        with self._windows(), patch("service._probe", return_value=self.catalog), \
                patch("service.process_identity", side_effect=[self.identity, self.identity, None]), \
                patch("service.process_alive", return_value=False), \
                patch("service.terminate_verified", return_value=True) as terminate:
            self.assertEqual(stop(self.config)["state"], "stopped")
        terminate.assert_called_once_with(123, self.identity)
        self.assertFalse(self.path.exists())

    def test_windows_final_guard_refusal_or_access_denial_retains_state(self):
        for result in (False, PermissionError("denied")):
            self._record()
            with self.subTest(result=result), self._windows(), \
                    patch("service._probe", return_value=self.catalog), \
                    patch("service.process_identity", return_value=self.identity), \
                    patch("service.terminate_verified", side_effect=result if isinstance(result, Exception) else None,
                          return_value=result):
                with self.assertRaisesRegex(RuntimeError, "identity changed|termination unavailable"):
                    stop(self.config)
                self.assertTrue(self.path.exists())

    def test_managed_state_is_bounded_and_pid_is_not_bool(self):
        for data in (b" " * (MAX_STATE_BYTES + 1), b'{"pid": true}', b'{"pid": 0}',
                     b'{"pid": 123,"pid": 124}'):
            with self.subTest(data=data[:60]):
                atomic_write(self.path, data)
                with self.assertRaises(ValueError):
                    status(self.config)

    def test_regular_state_log_and_lock_refuse_hard_link_aliases(self):
        target = self.root / "unrelated"
        target.write_bytes(b"do not modify")
        for leaf in ("mneme-codex-service.json", "mneme-mcp.log", "service.lock"):
            path = self.config.state_dir / leaf
            if path.exists():
                path.unlink()
            os.link(target, path)
            try:
                with self.subTest(leaf=leaf), patch("service._probe", return_value=None), \
                        patch("service.spawn", side_effect=AssertionError("unsafe path spawned")):
                    with self.assertRaisesRegex(ValueError, "multiply_linked"):
                        start(self.config)
                    self.assertEqual(target.read_bytes(), b"do not modify")
            finally:
                path.unlink()

    def test_regular_state_log_and_lock_refuse_symlinks(self):
        target = self.root / "unrelated"
        target.write_bytes(b"do not modify")
        for leaf in ("mneme-codex-service.json", "mneme-mcp.log", "service.lock"):
            path = self.config.state_dir / leaf
            if path.exists():
                path.unlink()
            try:
                path.symlink_to(target)
            except OSError as error:
                if os.name == "nt" and getattr(error, "winerror", None) == 1314:
                    self.skipTest("Windows symlink privilege unavailable")
                raise
            try:
                with self.subTest(leaf=leaf), patch("service._probe", return_value=None), \
                        patch("service.spawn", side_effect=AssertionError("unsafe path spawned")):
                    with self.assertRaises((OSError, ValueError)):
                        start(self.config)
                    self.assertEqual(target.read_bytes(), b"do not modify")
            finally:
                path.unlink()

    @unittest.skipUnless(os.name == "nt", "native Windows process handles and ACLs required")
    def test_native_windows_disposable_process_start_status_stop(self):
        class PythonServiceConfig(ServiceConfig):
            @property
            def argv(self):
                return [str(self.binary), "-c", "import time; time.sleep(30)"]

        config = PythonServiceConfig(Path(sys.executable), self.config.database_path,
                                     self.config.port, self.config.state_dir, self.root)
        # This is an owned disposable Python child, not a real MCP host or store.
        with patch("service._probe", side_effect=[None, self.catalog]):
            result = start(config)
        try:
            with patch("service._probe", return_value=self.catalog):
                self.assertEqual(status(config)["state"], "ready")
                self.assertEqual(stop(config)["state"], "stopped")
        finally:
            from host_process import terminate_verified
            if self.path.exists():
                identity = json.loads(self.path.read_text())["process_identity"]
                terminate_verified(result["pid"], identity)


class ConnectServiceTests(unittest.TestCase):
    def setUp(self):
        temp = tempfile.TemporaryDirectory()
        self.addCleanup(temp.cleanup)
        self.path = Path(temp.name) / "connect.json"
        self.data = {"mode": "connect", "url": "http://127.0.0.1:18767/",
                     "database_name": "user",
                     "database_path": "/home/user/.local/share/mneme/memory.db"}
        self.path.write_text(json.dumps(self.data))
        self.config = load_config(self.path)
        self.catalog = [{"db": "user", "name": "user", "state": "open",
                         "configured_path": self.data["database_path"]}]

    def no_local_control(self):
        stack = ExitStack()
        for name in ("_read_state", "_lock", "_matches_process", "subprocess.Popen",
                     "subprocess.run", "os.kill", "Path.is_file", "Path.resolve", "Path.mkdir"):
            stack.enter_context(patch("service." + name,
                                      side_effect=AssertionError("unexpected local control: " + name)))
        return stack

    def test_connect_config_retains_opaque_server_identity_without_local_files(self):
        self.assertIsInstance(self.config, ConnectConfig)
        self.assertIs(type(self.config.database_path), str)
        with self.no_local_control():
            self.assertEqual(self.config.validate(), self.config)
            self.assertEqual(self.config.database_path, self.data["database_path"])
        self.assertFalse(hasattr(self.config, "binary"))
        self.assertFalse(hasattr(self.config, "state_dir"))
        with self.assertRaisesRegex(ValueError, "connect-only"):
            ServiceConfig.from_json(self.path)

    def test_config_rejects_wrong_modes_mixed_fields_and_unnormalized_server_paths(self):
        invalid = []
        for key in self.data:
            missing = dict(self.data)
            del missing[key]
            invalid.append(missing)
        for key, value in (("mode", "remote"), ("mode", None), ("binary", "/bin/fake"),
                           ("project_db", "/tmp/local.db"), ("state_dir", "/tmp/state"),
                           ("port", 18767), ("extra", True), ("database_name", ["user"]),
                           ("database_name", "global"), ("token_env", "bad-name")):
            invalid.append(dict(self.data, **{key: value}))
        for value in ("relative", "//home/user/db", "/home/./user/db", "/home/user/../db",
                      "/home/user//db", "/home/user/db/", "/nul\x00db", "/" + "x" * 4096,
                      True, None, [], {}):
            invalid.append(dict(self.data, database_path=value))
        with self.no_local_control(), patch("service._probe", side_effect=AssertionError("unexpected HTTP")):
            for data in invalid:
                with self.subTest(data=data):
                    self.path.write_text(json.dumps(data))
                    with self.assertRaises(ValueError):
                        load_config(self.path)

    def test_connect_url_remains_numeric_loopback_http_root_only(self):
        invalid = (None, 18767, [], "https://127.0.0.1:18767/", "http://localhost:18767/",
                   "http://192.0.2.1:18767/", "http://127.0.0.1/", "http://127.0.0.1:80/",
                   "http://127.0.0.1:65536/", "http://127.0.0.1:notaport/", "http://[::1:18767/",
                   "http://user@127.0.0.1:18767/", "http://:secret@127.0.0.1:18767/",
                   "http://127.0.0.1:18767/other", "http://127.0.0.1:18767/?q=1",
                   "http://127.0.0.1:18767/#fragment", " http://127.0.0.1:18767/",
                   "http://127.0.0.1:18767/\n")
        for url in invalid:
            with self.subTest(url=url), self.assertRaises(ValueError):
                ConnectConfig(url, "user", self.config.database_path).validate_identity()
        ConnectConfig("http://[::1]:18767/", "user", self.config.database_path).validate_identity()

    def test_connect_config_duplicate_and_encoded_bounds_match_local_parser(self):
        self.path.write_text(json.dumps(self.data)[:-1] + ',"mode":"connect"}')
        with self.assertRaisesRegex(ValueError, "duplicate"):
            load_config(self.path)
        self.path.write_bytes(b" " * (MAX_CONFIG_BYTES + 1))
        with self.assertRaisesRegex(ValueError, "exceeds"):
            load_config(self.path)

    def test_status_and_readiness_only_probe_catalog_without_local_control(self):
        with self.no_local_control(), patch("service._probe", return_value=self.catalog) as probe:
            report = status(self.config)
            self.assertEqual(report["state"], "ready")
            self.assertEqual(report["mode"], "connect")
            self.assertEqual(report["db"], "user")
            self.assertEqual(report["database_path"], self.config.database_path)
            self.assertNotIn("pid", report)
            self.assertTrue(ensure_ready(self.config)["reuse_only"])
            self.assertEqual(probe.call_count, 2)

    def test_readiness_waits_only_for_matching_host_not_local_service_state(self):
        with self.no_local_control(), \
                patch("service._probe", side_effect=[None, self.catalog]) as probe:
            self.assertEqual(ensure_ready(self.config, timeout=0.5)["state"], "ready")
            self.assertEqual(probe.call_count, 2)

    def test_wrong_catalog_refuses_immediately_and_never_falls_back(self):
        invalid = [[], self.catalog * 2, {"databases": self.catalog}]
        for change in ({"db": "project"}, {"name": "project"}, {"state": "released"},
                       {"configured_path": "/Users/user/.local/share/mneme/memory.db"}):
            invalid.append([{**self.catalog[0], **change}])
        with self.no_local_control():
            for catalog in invalid:
                with self.subTest(catalog=catalog), patch("service._probe", return_value=catalog) as probe:
                    self.assertEqual(status(self.config)["state"], "unexpected-catalog")
                    with self.assertRaisesRegex(RuntimeError, "unexpected database catalog"):
                        ensure_ready(self.config, timeout=0.1)
                    self.assertEqual(probe.call_count, 2)

    def test_unreachable_tunnel_and_invalid_deadline_never_start_a_host(self):
        with self.no_local_control(), patch("service._probe", return_value=None) as probe:
            self.assertEqual(status(self.config)["state"], "unreachable")
            with self.assertRaisesRegex(RuntimeError, "SSH tunnel.*no local fallback"):
                ensure_ready(self.config, timeout=0.02)
            probe.reset_mock()
            for timeout in (True, None, "1", 0, -1, 31, float("nan"), float("inf")):
                with self.subTest(timeout=timeout), self.assertRaises(ValueError):
                    ensure_ready(self.config, timeout=timeout)
            probe.assert_not_called()

    def test_missing_token_refuses_before_probe_without_reflecting_token(self):
        config = ConnectConfig(self.config.url, "user", self.config.database_path, "MNEME_TEST_TOKEN")
        with self.no_local_control(), patch("service._probe", side_effect=AssertionError("unexpected HTTP")):
            for token in ("", "secret\nvalue", "secret\rvalue"):
                with self.subTest(token=token), patch.dict(os.environ, {"MNEME_TEST_TOKEN": token}):
                    with self.assertRaisesRegex(ValueError, "unset or invalid") as error:
                        ensure_ready(config)
                    self.assertNotIn("secret", str(error.exception))

    def test_management_functions_and_cli_refuse_connect_only_before_work(self):
        with self.no_local_control(), patch("service._probe", side_effect=AssertionError("unexpected HTTP")):
            for operation in (start, stop, restart, launchd_plist):
                with self.subTest(operation=operation.__name__), self.assertRaisesRegex(ValueError, "connect-only"):
                    operation(self.config)
            for action in ("start", "stop", "restart", "print-launchd"):
                error = io.StringIO()
                with self.subTest(action=action), redirect_stderr(error), self.assertRaises(SystemExit) as result:
                    main(["--config", str(self.path), action])
                self.assertEqual(result.exception.code, 2)
                self.assertIn("connect-only config supports status", error.getvalue())
        output = io.StringIO()
        with self.no_local_control(), patch("service._probe", return_value=self.catalog), redirect_stdout(output):
            main(["--config", str(self.path), "status"])
        self.assertEqual(json.loads(output.getvalue())["mode"], "connect")


class RemoteServiceTests(unittest.TestCase):
    def setUp(self):
        self.temp = tempfile.TemporaryDirectory()
        self.addCleanup(self.temp.cleanup)
        self.path = Path(self.temp.name) / "remote.json"
        self.data = {"mode": "remote", "schema": 1, "url": "ssh://memory@owner-alias:2222",
                     "database_name": "shared-notes", "database_path": "/srv/mneme/memory.db"}
        self.path.write_text(json.dumps(self.data))
        self.config = load_config(self.path)
        self.catalog = [{"db": "shared-notes", "name": "shared-notes", "state": "open",
                         "configured_path": self.data["database_path"]}]

    def no_local_control(self):
        stack = ExitStack()
        for name in ("_read_state", "_lock", "_matches_process", "subprocess.Popen",
                     "subprocess.run", "os.kill", "Path.is_file", "Path.resolve", "Path.mkdir"):
            stack.enter_context(patch("service." + name,
                                      side_effect=AssertionError("unexpected local control: " + name)))
        return stack

    def test_remote_config_is_closed_versioned_and_has_no_local_resources(self):
        self.assertIsInstance(self.config, RemoteConfig)
        self.assertEqual(self.config.ssh_mcp_port, 18766)
        with self.no_local_control():
            self.assertEqual(self.config.validate(), self.config)
        for field in ("binary", "port", "state_dir", "working_directory", "argv"):
            self.assertFalse(hasattr(self.config, field))
        invalid = []
        for key in self.data:
            value = dict(self.data)
            del value[key]
            invalid.append(value)
        for key, value in (("schema", True), ("schema", 2), ("schema", "1"),
                           ("binary", "/bin/x"), ("state_dir", "/tmp/state"),
                           ("working_directory", "/tmp"), ("project_db", "/tmp/db"),
                           ("port", 1234), ("db_id", "not-in-transport"),
                           ("database_name", "bad/name"), ("database_path", "../local.db"),
                           ("ssh_mcp_port", 0), ("ssh_mcp_port", True),
                           ("ssh_mcp_port", 65536), ("token_env", "bad-name")):
            invalid.append(dict(self.data, **{key: value}))
        with self.no_local_control():
            for data in invalid:
                with self.subTest(data=data):
                    self.path.write_text(json.dumps(data))
                    with self.assertRaises(ValueError):
                        load_config(self.path)

    def test_native_remote_factory_is_explicit_and_legacy_remains_local(self):
        configs = [self.config, ConnectConfig("http://127.0.0.1:18766/", "user", "/srv/memory.db")]
        for config in configs:
            with self.subTest(config=config), patch("mcp_client.McpClient") as factory:
                client_for_service(config, timeout=1.25, token="test-token")
                args, kwargs = factory.call_args
                self.assertEqual(args, (config.url,))
                self.assertEqual(kwargs["timeout"], 1.25)
                self.assertEqual(kwargs["token"], "test-token")
                self.assertEqual(kwargs.get("remote_owner", False), isinstance(config, RemoteConfig))
                if isinstance(config, RemoteConfig):
                    self.assertEqual(kwargs["ssh_mcp_port"], 18766)
                else:
                    self.assertNotIn("ssh_mcp_port", kwargs)

    def test_remote_probe_uses_native_factory_without_local_control(self):
        with self.no_local_control(), patch("service.client_for_service") as factory:
            factory.return_value.__enter__.return_value.call_tool.return_value = self.catalog
            self.assertEqual(status(self.config)["mode"], "remote")
            self.assertTrue(ensure_ready(self.config)["reuse_only"])
            self.assertEqual(factory.call_count, 2)
        with self.no_local_control(), patch("service._probe", return_value=self.catalog * 2):
            with self.assertRaisesRegex(RuntimeError, "unexpected database catalog"):
                ensure_ready(self.config)

    def test_remote_failure_never_starts_or_takes_over_local_service(self):
        with self.no_local_control(), patch("service._probe", return_value=None):
            with self.assertRaisesRegex(RuntimeError, "no local fallback"):
                ensure_ready(self.config, timeout=0.01)
        with self.no_local_control(), patch("service._probe", side_effect=AssertionError("unexpected probe")):
            for operation in (start, stop, restart, launchd_plist):
                with self.subTest(operation=operation), self.assertRaisesRegex(ValueError, "connect-only"):
                    operation(self.config)
            for action in ("start", "stop", "restart", "print-launchd"):
                with redirect_stderr(io.StringIO()), self.assertRaises(SystemExit):
                    main(["--config", str(self.path), action])

    def test_remote_token_requires_safe_transport_and_exact_native_token_shape(self):
        for url in ("http://owner.example/mcp", "http://localhost:18766"):
            with self.assertRaisesRegex(ValueError, "HTTPS"):
                RemoteConfig(url, "notes", "/srv/memory.db", "MNEME_TEST_TOKEN").validate_identity()
        for token in ("", "has space", "has\nnewline", "nonascii-é"):
            with patch.dict(os.environ, {"MNEME_TEST_TOKEN": token}), self.assertRaisesRegex(ValueError, "unset or invalid"):
                RemoteConfig("https://owner.example/mcp", "notes", "/srv/memory.db", "MNEME_TEST_TOKEN").validate()


if __name__ == "__main__":
    unittest.main()
