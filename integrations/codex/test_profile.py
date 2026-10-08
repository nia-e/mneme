import json
import hashlib
from pathlib import Path
import tempfile
import unittest
from unittest.mock import patch
from types import SimpleNamespace

import core_hook
import launcher
import profile


class ProfileTests(unittest.TestCase):
    def setUp(self):
        self.temp = tempfile.TemporaryDirectory()
        self.addCleanup(self.temp.cleanup)
        self.root = Path(self.temp.name).resolve()
        self.project = self.root / "project"
        (self.project / ".mneme").mkdir(parents=True)
        self.cwd = self.project / "src"
        self.cwd.mkdir()
        self.config = {"global_service": self.root / "personal.json",
                       "global_database": self.root / "personal.db",
                       "projects": [{"root": self.project,
                                     "service_config": self.root / "project.json"}]}

    def write_profile(self, mode, **extra):
        (self.project / ".mneme/profile.json").write_text(json.dumps({
            "schema": profile.SCHEMA, "mode": mode, **extra}))

    def test_absent_profile_preserves_global_then_project(self):
        self.assertEqual([target["db"] for target in core_hook._targets(self.config, str(self.cwd))],
                         ["user", "project"])

    def test_isolated_never_reaches_personal_collector(self):
        self.write_profile("isolated")
        targets = core_hook._targets(self.config, str(self.cwd))
        self.assertEqual([target["db"] for target in targets], ["project"])
        event = {"hook_event_name": "SessionStart", "source": "startup", "cwd": str(self.cwd)}
        with patch.object(core_hook, "collect_core", return_value=[] ) as collect:
            core_hook.handle_event(event, self.config)
        self.assertEqual(collect.call_args.args[0], targets)
        with self.assertRaisesRegex(ValueError, "forbids personal"):
            profile.permit_service(profile.select_for_cwd(self.cwd), "user", self.root / "personal.db")

    def test_invalid_profile_fails_before_collector(self):
        (self.project / ".mneme/profile.json").write_text('{"schema":"bad"}')
        event = {"hook_event_name": "SessionStart", "source": "startup", "cwd": str(self.cwd)}
        with patch.object(core_hook, "collect_core") as collect:
            with self.assertRaises(ValueError):
                core_hook.handle_event(event, self.config)
        collect.assert_not_called()

    def test_launcher_rejects_personal_before_host_start(self):
        self.write_profile("isolated")
        selected = profile.select_for_cwd(self.cwd)
        personal = SimpleNamespace(database_name="user", database_path=self.root / "personal.db")
        with (patch.object(launcher, "select_for_cwd", return_value=selected),
              patch.object(launcher, "load_config", return_value=personal),
              patch.object(launcher, "run_stream", side_effect=AssertionError("host contacted"))):
            with self.assertRaisesRegex(ValueError, "forbids personal"):
                launcher.main(["--service-config", str(self.root / "personal-service.json")])

    def test_selected_library_without_core_excludes_ambient_personal(self):
        library = self.root / "library.json"
        library.write_text(json.dumps({"schema": "mneme.library.config.v1"}))
        self.write_profile("default", library_config=str(library))
        self.assertEqual([target["db"] for target in core_hook._targets(self.config, str(self.cwd))],
                         ["project"])
        with self.assertRaisesRegex(ValueError, "ambient personal"):
            profile.permit_service(profile.select_for_cwd(self.cwd), "user", self.root / "personal.db")
        self.config["projects"] = []
        event = {"hook_event_name": "SessionStart", "source": "startup", "cwd": str(self.cwd)}
        with patch.object(core_hook, "collect_core", side_effect=AssertionError("collector started")):
            result = core_hook.handle_event(event, self.config)
        self.assertIn("no memory was loaded", result["hookSpecificOutput"]["additionalContext"])

    def test_private_keeps_selected_core_but_cannot_imply_sharing(self):
        library = self.root / "library.json"
        library.write_text(json.dumps({"schema": "mneme.library.config.v1", "core": {
            "global_service": str(self.root / "library-service.json"),
            "global_database": str(self.root / "library.db")}}))
        self.write_profile("private", library_config=str(library))
        targets = core_hook._targets(self.config, str(self.cwd))
        self.assertEqual([target["db"] for target in targets], ["user", "project"])
        self.assertEqual(targets[0]["database_path"], str(self.root / "library.db"))
        with self.assertRaises(ValueError):
            profile.permit_service(profile.select_for_cwd(self.cwd), "user", self.root / "personal.db")

    def test_isolated_requires_explicit_project_allowlist(self):
        self.write_profile("isolated")
        self.config["projects"] = []
        with self.assertRaisesRegex(ValueError, "explicitly selected"):
            core_hook._targets(self.config, str(self.cwd))

    def test_enrollment_requires_explicit_default_profile_and_verified_catalog(self):
        library_config = self.root / "library.json"
        library_config.write_text(json.dumps({"schema": "mneme.library.config.v1"}))
        self.write_profile("default", library_config=str(library_config))
        selection = profile.select_for_cwd(self.cwd)
        runtime = self.root / "runtime"
        (runtime / "config").mkdir(parents=True)
        (runtime / "lib").mkdir()
        helper = runtime / "lib/library.py"
        receipt = self.root / "enrolled.txt"
        helper.write_text("from pathlib import Path\n"
                          "def enroll_project(config, root, db_id, *, database, owner_endpoint):\n"
                          "    assert database == 'project'\n"
                          "    assert owner_endpoint['url'] == 'http://127.0.0.1:1/'\n"
                          f"    Path({str(receipt)!r}).write_text(db_id)\n")
        service = runtime / "config/service.json"
        service.write_text("{}")
        marker = runtime / "config/enrollment.json"
        marker.write_text(json.dumps({"mode": "fresh", "project_root": str(self.project),
                                      "helper_sha256": hashlib.sha256(helper.read_bytes()).hexdigest()}))
        config = SimpleNamespace(database_name="project", database_path=self.project / ".mneme/codex-memory.db",
                                 token_env="", url="http://127.0.0.1:1/")
        catalog = [{"db": "project", "name": "project", "state": "open",
                    "configured_path": str(config.database_path),
                    "db_id": "01ARZ3NDEKTSV4RRFFQ69G5FAV"}]
        class Client:
            def __init__(self, *args, **kwargs):
                pass
            def __enter__(self):
                return self
            def __exit__(self, *args):
                pass
            def call_tool(self, name, args):
                self_name.append(name)
                return catalog
        self_name = []
        with patch.object(launcher, "McpClient", Client):
            launcher._enroll_selected_project(config, selection, service)
        self.assertEqual(self_name, ["databases"])
        self.assertEqual(receipt.read_text(), catalog[0]["db_id"])
        receipt.unlink()
        self.write_profile("private", library_config=str(library_config))
        with patch.object(launcher, "McpClient", side_effect=AssertionError("private contacted host")):
            launcher._enroll_selected_project(config, profile.select_for_cwd(self.cwd), service)
        self.assertFalse(receipt.exists())


if __name__ == "__main__":
    unittest.main()
