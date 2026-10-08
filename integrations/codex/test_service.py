from contextlib import ExitStack, redirect_stderr, redirect_stdout
import io
import json
import os
from pathlib import Path
import plistlib
import signal
import subprocess
import tempfile
import threading
import unittest
from concurrent.futures import ThreadPoolExecutor
from unittest.mock import Mock, patch

from service import (ConnectConfig, MAX_CONFIG_BYTES, ServiceConfig, _expected_catalog,
                     _matches_process, _state_matches_config, ensure_ready, launchd_plist,
                     load_config, main, restart, start, status, stop)


class ServiceTests(unittest.TestCase):
    def setUp(self):
        self.temp = tempfile.TemporaryDirectory()
        self.addCleanup(self.temp.cleanup)
        root = Path(self.temp.name)
        binary = root / "isolated" / "bin" / "mneme-mcp"
        binary.parent.mkdir(parents=True)
        binary.write_bytes(b"not run")
        binary.chmod(0o700)
        state_dir = root / "state"
        state_dir.mkdir(mode=0o700)
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
        for name in ("user", "project"):
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
        for name in ("", "global", "User", None, True, ["user"]):
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
        invalid = self._named_config(database_name="global")
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
        bad = ServiceConfig(Path("mneme-mcp"), self.config.project_db,
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
        binary = self.config.binary.parent / "mneme mcp"
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


if __name__ == "__main__":
    unittest.main()
