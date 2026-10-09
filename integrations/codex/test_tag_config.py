"""Pure tag config admission and prepare defaults; no live owners or models."""
import copy
import unittest
import json
import tempfile
from pathlib import Path
from unittest.mock import patch
import hooks
import recording_jobs
import install
import tag_config
from install_test_support import InstallFixture
import subprocess
import sys
import misc_config


class TagConfigTests(unittest.TestCase):
    def test_existing_absence_stays_disabled(self):
        self.assertEqual(tag_config.validate({"recording_mode": "automatic"}), (False, None))

    def test_prepare_defaults_and_explicit_off(self):
        self.assertTrue(tag_config.prepared_fields("automatic")["tag_stewardship"])
        self.assertFalse(tag_config.prepared_fields("off")["tag_stewardship"])
        self.assertFalse(tag_config.prepared_fields("automatic", False)["tag_stewardship"])

    def test_reject_wrong_types_noncanonical_ids_and_keep_parent_off_inert(self):
        cases = [{"tag_stewardship": x} for x in (0, 1, "true", None)]
        cases += [{"tag_guide_id": x} for x in (1, "", "8" * 26, "i" * 26, "../guide", "0" * 25)]
        for case in cases:
            with self.subTest(case=case), self.assertRaises(ValueError):
                tag_config.validate({"recording_mode": "automatic", **case})
        self.assertTrue(tag_config.prepared_fields("off", True)["tag_stewardship"])
        self.assertEqual(tag_config.validate({"recording_mode": "off", "tag_stewardship": True}), (True, None))
        self.assertEqual(tag_config.validate({"recording_mode": "automatic", "tag_guide_id": "1" * 26}), (False, "1" * 26))

    def test_fresh_project_prepare_defaults_and_explicit_disable(self):
        fixture = InstallFixture()
        self.addCleanup(fixture.temp.cleanup)
        plan = fixture.async_plan(recording_mode="automatic")
        self.assertTrue(plan["tag_stewardship"])
        self.assertIsNone(plan["tag_guide_id"])
        options = dict(program_root=fixture.sources, recall_mode="async", recording_mode="automatic",
                       reader_model="gpt-6.1-sol", reader_codex=fixture.sources / "codex")
        disabled = install.prepare(fixture.project, fixture.prefix, fixture.sources / "mnemed",
                                   fixture.sources / "mneme-mcp", 18765, **options,
                                   tag_stewardship=False, tag_guide_id="1" * 26)
        self.assertFalse(disabled["tag_stewardship"])
        self.assertEqual(disabled["tag_guide_id"], "1" * 26)
        self.assertFalse(fixture.prefix.exists())
        self.assertFalse((fixture.project / ".mneme").exists())

    def test_hook_config_load_is_read_only_and_does_not_enable_old_config(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            state = root / "absent-state"
            config = root / "hooks.json"
            data = {"schema": "mneme.codex-hooks.config.v8", "project_root": str(root),
                    "state_dir": str(state), "service_config": str(root / "service.json"),
                    "memory_mode": "async", "reader_model": "gpt-6.1-sol",
                    "reader_codex": "/fixture/codex", "reader_codex_sha256": "a" * 64,
                    "recording_mode": "automatic", "librarian_effort": "medium"}
            for fields in ({}, {"tag_stewardship": True, "tag_guide_id": "1" * 26},
                           {"tag_stewardship": False, "tag_guide_id": None}):
                config.write_text(json.dumps({**data, **fields}))
                before = config.read_bytes()
                with patch.object(recording_jobs, "load_project_focus", return_value=None), \
                     patch.object(recording_jobs, "_config_digest", return_value="b" * 64):
                    loaded = hooks._config(config)
                self.assertEqual(loaded.get("tag_stewardship", False), fields.get("tag_stewardship", False))
                self.assertFalse(state.exists())
                self.assertEqual(config.read_bytes(), before)
                self.assertEqual(set(root.iterdir()), {config})
            config.write_text(json.dumps({**data, "recording_mode": "off", "tag_stewardship": True}))
            with patch.object(recording_jobs, "load_project_focus", return_value=None), \
                 patch.object(recording_jobs, "_config_digest", return_value="b" * 64):
                loaded = hooks._config(config)
            self.assertEqual(loaded["recording_mode"], "off")
            self.assertTrue(loaded["tag_stewardship"])
            self.assertFalse(state.exists())

    def test_packaging_inventory_and_old_runtime_configs(self):
        for name in ("tag_config.py", "tag_context.py", "stewardship.py", "stewardship_contract.py"):
            self.assertIn(name, install.PROGRAMS)
            self.assertIn(("lib/" + name, False), install.DESTINATIONS)
            self.assertNotIn(("lib/" + name, False), install.V20_DESTINATIONS)
        fixture = InstallFixture()
        self.addCleanup(fixture.temp.cleanup)
        plan = fixture.prepare()
        plan.update(recall_mode="async", librarian_effort="medium", recording_mode="automatic",
                    reader_model="gpt-6.1-sol", reader_codex={"source": "/fixture/codex", "sha256": "a" * 64})
        _, old = install.runtime_configs(plan)
        self.assertNotIn("tag_stewardship", old)
        plan.update(tag_stewardship=True, tag_guide_id="1" * 26)
        _, new = install.runtime_configs(plan)
        self.assertTrue(new["tag_stewardship"])
        self.assertEqual(new["tag_guide_id"], "1" * 26)

    def test_misc_prepare_and_existing_static_compatibility(self):
        fixture = InstallFixture()
        self.addCleanup(fixture.temp.cleanup)
        service = fixture.base / "service.json"
        service.write_text(json.dumps({"mode": "connect", "url": "http://127.0.0.1:18765/",
                                      "database_name": "project", "database_path": str(fixture.base / "misc.db")}))
        config = misc_config.prepare(state_dir=str(fixture.base / "state"), service_config=str(service),
            database_path=str(fixture.base / "misc.db"), db_id="1" * 26,
            reader_codex=str(fixture.sources / "codex"), hooks_program=hooks.__file__,
            hook_config_path=str(fixture.base / "device-hooks.json"))["hook_config"]
        self.assertTrue(config["tag_stewardship"])
        old = copy.deepcopy(config)
        old.pop("tag_stewardship"); old.pop("tag_guide_id")
        self.assertNotIn("tag_stewardship", misc_config.validate_config(old))
        self.assertFalse((fixture.base / "state").exists())
        self.assertFalse((fixture.base / "misc.db").exists())

    def test_copied_package_imports_and_retained_actual_inventories(self):
        fixture = InstallFixture()
        self.addCleanup(fixture.temp.cleanup)
        plan = fixture.async_plan(recording_mode="automatic", program_root=Path(install.__file__).parent)
        install.apply(plan)
        copied = fixture.prefix / "lib"
        for row in plan["files"]:
            self.assertEqual(install.digest(Path(row["source"]).read_bytes()),
                             install.digest((fixture.prefix / row["destination"]).read_bytes()))
        result = subprocess.run([sys.executable, "-c",
            "import hooks, tag_config, tag_context, stewardship, stewardship_contract; "
            "assert tag_config.validate({'recording_mode':'automatic'}) == (False,None)"],
            cwd=copied, capture_output=True, text=True, timeout=10)
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertFalse((fixture.project / ".mneme" / "codex-hook-state").exists())
        self.assertFalse((fixture.project / ".mneme" / "codex-memory.db").exists())
        for revision in (20, 21):
            retained = copy.deepcopy(plan)
            retained["config_revision"] = revision
            retained.pop("tag_stewardship"); retained.pop("tag_guide_id")
            retained["files"] = retained["files"][:len(install.V20_DESTINATIONS)]
            if revision == 21:
                service = fixture.base / "retained-service.json"
                service.write_text(json.dumps({"binary": str(fixture.sources / "mneme-mcp"),
                    "project_db": str(fixture.project / ".mneme/codex-memory.db"),
                    "working_directory": str(fixture.project), "port":18765,
                    "state_dir": str(fixture.base / "old-run")}))
                retained["existing_service_config"] = install.existing_service_input(service)
            retained["plan_sha256"] = install.plan_hash(retained)
            install.validate_plan(retained)
            self.assertEqual([(row["destination"], row["executable"]) for row in retained["files"]],
                             list(install.V20_DESTINATIONS))
            _, old = install.runtime_configs(retained)
            self.assertNotIn("tag_stewardship", old)



if __name__ == "__main__":
    unittest.main()
