import json
import copy
from pathlib import Path
import tempfile
import unittest
import sys
import subprocess
if sys.version_info < (3, 11):
    raise unittest.SkipTest("Installer tests require Python 3.11+; run the full suite with a supported interpreter")
import tomllib
from unittest.mock import patch

import install
from install_test_support import InstallFixture


class InstallTests(unittest.TestCase):
    def test_shared_async_handler_builder_keeps_exact_lifecycle_shape(self):
        events = install.hook_handlers("python hook_launcher.py --config device.json", async_mode=True, recording=True)
        self.assertEqual(set(events), {"SessionStart", "UserPromptSubmit", "PostToolUse", "Stop", "Interrupt", "SessionEnd"})
        self.assertEqual(len(events["UserPromptSubmit"]), 2)
        self.assertTrue(events["UserPromptSubmit"][1]["hooks"][0]["async"])
        self.assertEqual(events["PostToolUse"][0]["hooks"][0]["additionalContextLimit"], 0)
        self.assertEqual(events["SessionEnd"][0]["hooks"][0]["timeout"], 3)
        self.assertEqual(len(events["SessionEnd"]), 1)

    def test_retained_revision19_plan_keeps_its_exact_module_inventory(self):
        current = self.plan()
        old = copy.deepcopy(current)
        old["config_revision"] = 19
        old.pop("tag_stewardship", None)
        old.pop("tag_guide_id", None)
        old["files"] = old["files"][:len(install.V19_DESTINATIONS)]
        old["plan_sha256"] = install.plan_hash(old)
        install.validate_plan(old)
        self.assertNotEqual(len(old["files"]), len(install.DESTINATIONS))
        before = {"config.toml": None, "hooks.json": None}
        self.assertEqual(install.config_outputs(old, before), install.config_outputs(current, before))

    def setUp(self):
        fixture = InstallFixture()
        self.temp = fixture.temp
        self.addCleanup(self.temp.cleanup)
        for name in ("base", "project", "prefix", "sources"):
            setattr(self, name, getattr(fixture, name))

    def historic(self, plan):
        plan.pop("tag_stewardship", None)
        plan.pop("tag_guide_id", None)
        plan.pop('librarian_effort', None)
        plan['files'] = plan['files'][:len(install.V13_DESTINATIONS)]
        if plan.get('recall_mode') == 'async':
            plan['reader_model'] = 'gpt-5.6-sol'

    def plan(self):
        return install.prepare(self.project, self.prefix, self.sources / 'mnemed',
                               self.sources / 'mneme-mcp', 18765, program_root=self.sources)

    def existing_owner(self, **changes):
        config = {"binary": str(self.sources / "mneme-mcp"),
                  "project_db": str(self.project / ".mneme/codex-memory.db"),
                  "working_directory": str(self.project), "port": 18765,
                  "state_dir": str(self.base / "old-owner-state")}
        config.update(changes)
        path = self.base / "old-service.json"
        path.write_bytes(install.json_bytes(config))
        return path, config

    def existing_plan(self, path):
        return install.prepare(self.project, self.prefix, self.sources / "mnemed",
                               self.sources / "mneme-mcp", 18765, program_root=self.sources,
                               existing_service_config=path)

    def test_explicit_existing_owner_retains_exact_config_and_reuses_without_start(self):
        import service
        path, owner = self.existing_owner()
        db = Path(owner["project_db"])
        db.parent.mkdir()
        db.write_bytes(b"disposable mocked store, never opened")
        state_dir = Path(owner["state_dir"])
        state_dir.mkdir(mode=0o700)
        state = {key: value for key, value in owner.items() if key != "state_dir"}
        state["pid"] = 123
        state_path = state_dir / "mneme-codex-service.json"
        state_path.write_text(json.dumps(state))
        original_state = state_path.read_bytes()
        with patch("service._probe") as probe, patch("service.subprocess.Popen") as spawn:
            plan = self.existing_plan(path)
            self.assertEqual(plan["config_revision"], install.EXISTING_SERVICE_REVISION)
            self.assertEqual(plan["existing_service_config"]["config"], owner)
            install.apply(plan)
            probe.assert_not_called()
            spawn.assert_not_called()
        installed_path = self.prefix / "config/service.json"
        self.assertEqual(json.loads(installed_path.read_text()), owner)
        self.assertEqual(state_path.read_bytes(), original_state)
        self.assertEqual(list((self.prefix / "run").iterdir()), [])
        catalog = [{"db": "project", "name": "project", "state": "open",
                    "configured_path": owner["project_db"]}]
        with patch("service._matches_process", return_value=True), \
                patch("service._probe", return_value=catalog), \
                patch("service.subprocess.Popen") as spawn:
            self.assertEqual(service.ensure_ready(service.load_config(installed_path))["state"], "ready")
            spawn.assert_not_called()
        self.assertEqual(state_path.read_bytes(), original_state)

    def test_existing_owner_accepts_exact_named_project_schema(self):
        path, data = self.existing_owner()
        data["database_path"] = data.pop("project_db")
        data["database_name"] = "project"
        path.write_bytes(install.json_bytes(data))
        self.assertEqual(install.runtime_configs(self.existing_plan(path))[0], data)

    def test_cli_exposes_and_prepares_explicit_existing_owner_without_side_effects(self):
        path, _ = self.existing_owner()
        output = self.base / "reviewed-plan.json"
        command = [sys.executable, install.__file__, "prepare",
                   "--project-root", str(self.project), "--prefix", str(self.prefix),
                   "--mnemed", str(self.sources / "mnemed"), "--mcp", str(self.sources / "mneme-mcp"),
                   "--port", "18765", "--existing-service-config", str(path), "--output", str(output)]
        help_result = subprocess.run([sys.executable, install.__file__, "prepare", "--help"],
                                     capture_output=True, text=True, timeout=10, check=True)
        self.assertIn("--existing-service-config", help_result.stdout)
        result = subprocess.run(command, capture_output=True, text=True, timeout=10, check=True)
        self.assertEqual(json.loads(result.stdout)["status"], "prepared")
        prepared = json.loads(output.read_text())
        install.validate_plan(prepared)
        self.assertEqual(prepared["config_revision"], install.EXISTING_SERVICE_REVISION)
        self.assertFalse(self.prefix.exists())
        self.assertFalse((self.project / ".codex").exists())

    def test_existing_owner_rejects_foreign_mixed_connect_and_unbounded_inputs(self):
        cases = [
            {"project_db": "/foreign.db"}, {"working_directory": "/foreign"},
            {"port": True}, {"port": 19876}, {"binary": str(self.sources / "mnemed")},
            {"database_name": "user", "database_path": "/foreign.db"},
            {"mode": "connect", "url": "http://127.0.0.1:18765/"},
            {"unknown": True}, {"state_dir": "relative"},
            {"state_dir": str(self.base) + "/../foreign"}, {"state_dir": ["/bad"]},
            {"state_dir": "/nul\x00state"},
        ]
        for changes in cases:
            with self.subTest(changes=changes):
                path, _ = self.existing_owner(**changes)
                with self.assertRaises((ValueError, TypeError)):
                    self.existing_plan(path)
                self.assertFalse(self.prefix.exists())
        path, data = self.existing_owner()
        data["database_path"] = data.pop("project_db")
        data["database_name"] = "user"
        path.write_bytes(install.json_bytes(data))
        with self.assertRaisesRegex(ValueError, "exact project"):
            self.existing_plan(path)
        path.write_bytes(b" " * 32769)
        with self.assertRaisesRegex(ValueError, "exceeds"):
            self.existing_plan(path)
        path.write_text('{"binary":"/one","binary":"/two"}')
        with self.assertRaisesRegex(ValueError, "duplicate"):
            self.existing_plan(path)
        path.unlink()
        path.symlink_to(self.sources / "mnemed")
        with self.assertRaisesRegex(ValueError, "canonical"):
            self.existing_plan(path)
        with self.assertRaisesRegex(ValueError, "regular"):
            self.existing_plan(self.sources)

    def test_existing_owner_config_and_native_drift_denied_before_apply_writes(self):
        for target in ("config", "binary"):
            with self.subTest(target=target):
                path, _ = self.existing_owner()
                native = self.sources / "mneme-mcp"
                native.write_text("# original native\n")
                plan = self.existing_plan(path)
                (path if target == "config" else native).write_text("# changed\n")
                with self.assertRaises(ValueError), patch("install.private_write") as write:
                    install.apply(plan)
                write.assert_not_called()
                self.assertFalse(self.prefix.exists())
                self.assertFalse((self.project / ".codex").exists())

    def test_revision21_pin_presence_and_shape_are_fenced_but_uninstall_ignores_live_drift(self):
        path, _ = self.existing_owner()
        plan = self.existing_plan(path)
        for revision, pin in ((20, plan["existing_service_config"]), (21, None),
                              (21, {**plan["existing_service_config"], "unknown": True})):
            candidate = copy.deepcopy(plan)
            candidate["config_revision"] = revision
            if pin is None:
                candidate.pop("existing_service_config")
            else:
                candidate["existing_service_config"] = pin
            candidate["plan_sha256"] = install.plan_hash(candidate)
            with self.assertRaises(ValueError):
                install.validate_plan(candidate)
        result = install.apply(plan)
        path.unlink()
        (self.sources / "mneme-mcp").unlink()
        install.uninstall(result["receipt"])
        self.assertFalse((self.project / ".codex/config.toml").exists())

    def test_revision20_receipt_outputs_and_uninstall_remain_unchanged(self):
        plan = self.plan()
        plan["config_revision"] = 20
        plan.pop("tag_stewardship", None)
        plan.pop("tag_guide_id", None)
        plan["files"] = plan["files"][:len(install.V20_DESTINATIONS)]
        plan["plan_sha256"] = install.plan_hash(plan)
        self.assertEqual(plan["config_revision"], 20)
        self.assertNotIn("existing_service_config", plan)
        originals = {"config.toml": None, "hooks.json": None}
        outputs = install.config_outputs(plan, originals)
        result = install.apply(plan)
        self.assertEqual(json.loads(Path(result["receipt"]).read_text())["installed_configs"],
                         install._output_hashes(outputs))
        install.uninstall(result["receipt"])
        self.assertFalse((self.project / ".codex/hooks.json").exists())

    def shadow_plan(self):
        codex = self.sources / 'codex'
        codex.write_text('#!/bin/sh\nexit 0\n')
        codex.chmod(0o700)
        return install.prepare(self.project, self.prefix, self.sources / 'mnemed',
                               self.sources / 'mneme-mcp', 18765, program_root=self.sources,
                               recall_mode='shadow', shadow_model='gpt-5.6-sol', shadow_codex=codex)

    def async_plan(self, *, recording_mode="off"):
        codex = self.sources / 'codex'
        codex.write_text('#!/bin/sh\nexit 0\n')
        codex.chmod(0o700)
        return install.prepare(self.project, self.prefix, self.sources / 'mnemed',
                               self.sources / 'mneme-mcp', 18765, program_root=self.sources,
                               recall_mode='async', reader_model='gpt-6.1-sol', reader_codex=codex,
                               recording_mode=recording_mode)

    def test_prepare_has_no_project_or_prefix_side_effects(self):
        plan = self.plan()
        self.assertEqual(list(self.project.iterdir()), [])
        self.assertFalse(self.prefix.exists())
        self.assertEqual(plan['project_db'], str(self.project / '.mneme/codex-memory.db'))
        self.assertEqual(plan['config_revision'], install.CONFIG_REVISION)
        self.assertEqual(plan['recording_mode'], 'off')
        self.assertEqual(plan['recall_mode'], 'reminder')
        self.assertIsNone(plan['shadow_model'])
        self.assertIsNone(plan['shadow_codex'])
        self.assertIsNone(plan['reader_model'])
        self.assertIsNone(plan['reader_codex'])
        self.assertEqual([row['destination'] for row in plan['files']],
                         [destination for destination, _ in install.DESTINATIONS])

    def test_library_helper_is_pinned_and_existing_store_needs_explicit_enrollment(self):
        helper = self.sources / 'library.py'
        helper.write_text('# fixture helper\n')
        fresh = install.prepare(self.project, self.prefix, self.sources / 'mnemed',
                                self.sources / 'mneme-mcp', 18765,
                                program_root=self.sources, library_helper=helper)
        self.assertEqual(fresh['library_enrollment'], 'fresh')
        self.assertEqual(fresh['library_helper']['sha256'], install.digest(helper.read_bytes()))
        result = install.apply(fresh)
        self.assertEqual((self.prefix / 'lib/library.py').read_bytes(), helper.read_bytes())
        self.assertEqual(json.loads((self.prefix / 'config/enrollment.json').read_text())['mode'], 'fresh')
        install.uninstall(result['receipt'])

        # A different destination allows another reviewable plan; the store
        # itself is never created or opened by prepare.
        (self.project / '.mneme').mkdir(exist_ok=True)
        (self.project / '.mneme/codex-memory.db').write_bytes(b'fixture')
        with self.assertRaisesRegex(ValueError, 'requires --library-helper'):
            install.prepare(self.project, self.prefix.with_name('runtime2'),
                            self.sources / 'mnemed', self.sources / 'mneme-mcp', 18765,
                            program_root=self.sources, enroll_existing=True)
        plain = install.prepare(self.project, self.prefix.with_name('runtime2'),
                                self.sources / 'mnemed', self.sources / 'mneme-mcp', 18765,
                                program_root=self.sources, library_helper=helper)
        self.assertEqual(plain['library_enrollment'], 'none')
        self.assertIsNone(plain['library_helper'])
        install.validate_plan(plain)
        explicit = install.prepare(self.project, self.prefix.with_name('runtime2'),
                                   self.sources / 'mnemed', self.sources / 'mneme-mcp', 18765,
                                   program_root=self.sources, library_helper=helper,
                                   enroll_existing=True)
        self.assertEqual(explicit['library_enrollment'], 'existing')

    def test_rev5_receipt_shape_remains_known_but_old_plan_cannot_apply(self):
        plan = self.plan()
        plan['config_revision'] = 5
        plan.pop("tag_stewardship", None)
        plan.pop("tag_guide_id", None)
        self.historic(plan)
        plan['files'] = plan['files'][:9]  # No library wrapper, shadow, or async helpers.
        plan.pop('shadow_model')
        plan.pop('shadow_codex')
        plan.pop('reader_model')
        plan.pop('reader_codex')
        plan.pop('recording_mode')
        plan['plan_sha256'] = install.plan_hash(plan)
        install.validate_plan(plan)
        with self.assertRaisesRegex(ValueError, 'prepare a new plan'):
            install.apply(plan)

    def test_rev6_output_retains_exact_legacy_tools_and_cannot_apply(self):
        plan = self.plan()
        plan['config_revision'] = 6
        plan.pop("tag_stewardship", None)
        plan.pop("tag_guide_id", None)
        self.historic(plan)
        plan['files'] = plan['files'][:10]
        plan.pop('shadow_model')
        plan.pop('shadow_codex')
        plan.pop('reader_model')
        plan.pop('reader_codex')
        plan.pop('recording_mode')
        plan['plan_sha256'] = install.plan_hash(plan)
        install.validate_plan(plan)
        output = install.config_outputs(plan, {'config.toml': None, 'hooks.json': None})
        server = tomllib.loads(output['config.toml'].decode())['mcp_servers']['mneme_project']
        self.assertEqual(server['enabled_tools'], install.LEGACY_TOOLS)
        self.assertEqual(set(server['tools']), set(install.LEGACY_TOOLS))
        self.assertNotIn('episode', server['tools'])
        with self.assertRaisesRegex(ValueError, 'prepare a new plan'):
            install.apply(plan)

    def test_rev6_receipt_uninstall_keeps_its_five_tool_output(self):
        plan = self.plan()
        result = install.apply(plan)
        plan['config_revision'] = 6
        plan.pop("tag_stewardship", None)
        plan.pop("tag_guide_id", None)
        self.historic(plan)
        plan['files'] = plan['files'][:10]
        plan.pop('shadow_model')
        plan.pop('shadow_codex')
        plan.pop('reader_model')
        plan.pop('reader_codex')
        plan.pop('recording_mode')
        plan['plan_sha256'] = install.plan_hash(plan)
        outputs = install.config_outputs(plan, {'config.toml': None, 'hooks.json': None})
        for name, data in outputs.items():
            (self.project / '.codex' / name).write_bytes(data)
        receipt_path = Path(result['receipt'])
        receipt = json.loads(receipt_path.read_text())
        receipt['plan'] = plan
        receipt['installed_configs'] = install._output_hashes(outputs)
        receipt_path.write_text(json.dumps(receipt))
        self.assertEqual(install.uninstall(receipt_path)['status'], 'configuration removed')

    def test_rev7_receipt_uninstall_keeps_its_pre_shadow_inventory(self):
        current = self.plan()
        result = install.apply(current)
        old = copy.deepcopy(current)
        old['config_revision'] = 7
        old.pop("tag_stewardship", None)
        old.pop("tag_guide_id", None)
        self.historic(old)
        old['files'] = old['files'][:10]
        old.pop('shadow_model')
        old.pop('shadow_codex')
        old.pop('reader_model')
        old.pop('reader_codex')
        old.pop('recording_mode')
        old['plan_sha256'] = install.plan_hash(old)
        install.validate_plan(old)
        receipt_path = Path(result['receipt'])
        receipt = json.loads(receipt_path.read_text())
        receipt['plan'] = old
        output = install.config_outputs(old, {'config.toml': None, 'hooks.json': None})
        for name, data in output.items():
            (self.project / '.codex' / name).write_bytes(data)
        receipt['installed_configs'] = install._output_hashes(output)
        receipt_path.write_text(json.dumps(receipt))
        self.assertEqual(install.uninstall(receipt_path)['status'], 'configuration removed')

    def test_rev8_shadow_receipt_uninstalls_with_original_output(self):
        current = self.shadow_plan()
        result = install.apply(current)
        old = copy.deepcopy(current)
        old['config_revision'] = 8
        old.pop("tag_stewardship", None)
        old.pop("tag_guide_id", None)
        self.historic(old)
        old['files'] = old['files'][:11]
        old.pop('reader_model')
        old.pop('reader_codex')
        old.pop('recording_mode')
        old['plan_sha256'] = install.plan_hash(old)
        install.validate_plan(old)
        original = {'config.toml': None, 'hooks.json': None}
        output = install.config_outputs(old, original)
        for name, data in output.items():
            (self.project / '.codex' / name).write_bytes(data)
        receipt = Path(result['receipt'])
        body = json.loads(receipt.read_text())
        body['plan'] = old
        body['installed_configs'] = install._output_hashes(output)
        receipt.write_text(json.dumps(body))
        self.assertEqual(install.uninstall(receipt)['status'], 'configuration removed')
        self.assertFalse((self.project / '.codex/hooks.json').exists())

    def test_rev8_pending_recovers_original_shadow_output(self):
        old = self.shadow_plan()
        old['config_revision'] = 8
        old.pop("tag_stewardship", None)
        old.pop("tag_guide_id", None)
        self.historic(old)
        old['files'] = old['files'][:11]
        old.pop('reader_model')
        old.pop('reader_codex')
        old.pop('recording_mode')
        old['plan_sha256'] = install.plan_hash(old)
        expected = install.config_outputs(old, {'config.toml': None, 'hooks.json': None})
        config_dir = self.project / '.codex'
        config_dir.mkdir()
        for name, data in expected.items():
            (config_dir / name).write_bytes(data)
        self.prefix.mkdir()
        pending = self.prefix / 'pending.json'
        pending.write_text(json.dumps(old))
        self.assertEqual(install.recover(pending)['status'], 'aborted')
        self.assertFalse((config_dir / 'config.toml').exists())
        self.assertFalse((config_dir / 'hooks.json').exists())

    def test_all_historical_plan_inventories_and_reminder_outputs_remain_valid(self):
        expected = {
            1: install.LEGACY_DESTINATIONS, 2: install.LEGACY_DESTINATIONS,
            3: install.V3_DESTINATIONS, 4: install.V4_DESTINATIONS,
            5: install.V5_DESTINATIONS, 6: install.V7_DESTINATIONS,
            7: install.V7_DESTINATIONS, 8: install.V8_DESTINATIONS,
        }
        current = self.plan()
        for revision, inventory in expected.items():
            with self.subTest(revision=revision):
                old = copy.deepcopy(current)
                old['files'] = old['files'][:len(inventory)]
                old.pop('reader_model')
                old.pop('reader_codex')
                old.pop('recording_mode')
                if revision < 8:
                    old.pop('shadow_model')
                    old.pop('shadow_codex')
                if revision < 5:
                    old.pop('library_helper')
                    old.pop('library_enrollment')
                if revision < 4:
                    old.pop('recall_mode')
                if revision == 1:
                    old.pop('config_revision')
                    self.historic(old)
                else:
                    old['config_revision'] = revision
                    old.pop("tag_stewardship", None)
                    old.pop("tag_guide_id", None)
                    self.historic(old)
                old['plan_sha256'] = install.plan_hash(old)
                install.validate_plan(old)
                outputs = install.config_outputs(old, {'config.toml': None, 'hooks.json': None})
                hooks = json.loads(outputs['hooks.json'])['hooks']
                self.assertEqual(set(hooks), {'SessionStart', 'UserPromptSubmit', 'Stop'})
                self.assertEqual(len(old['files']), len(inventory))

    def test_shadow_requires_explicit_model_and_pinned_absolute_codex(self):
        for options, message in [
            ({'recall_mode': 'shadow'}, '--shadow-model'),
            ({'recall_mode': 'shadow', 'shadow_model': 'gpt-5.6-sol'}, 'absolute --shadow-codex'),
            ({'shadow_model': 'gpt-5.6-sol'}, 'require --recall-mode shadow'),
        ]:
            with self.subTest(options=options), self.assertRaisesRegex(ValueError, message):
                install.prepare(self.project, self.prefix, self.sources / 'mnemed',
                                self.sources / 'mneme-mcp', 18765, program_root=self.sources,
                                **options)
        codex = self.sources / 'codex'
        codex.write_text('# fixture\n')
        codex.chmod(0o700)
        with self.assertRaisesRegex(ValueError, 'absolute --shadow-codex'):
            install.prepare(self.project, self.prefix, self.sources / 'mnemed',
                            self.sources / 'mneme-mcp', 18765, program_root=self.sources,
                            recall_mode='shadow', shadow_model='gpt-5.6-sol',
                            shadow_codex=Path('codex'))
        self.assertFalse(self.prefix.exists())

    def test_shadow_bundle_is_pinned_without_copying_external_codex(self):
        plan = self.shadow_plan()
        self.assertEqual(plan['shadow_codex'], {
            'source': str((self.sources / 'codex').resolve()),
            'sha256': install.digest((self.sources / 'codex').read_bytes()),
        })
        self.assertEqual(plan['files'][10]['destination'], 'lib/shadow.py')
        shadow = next(row for row in plan['files'] if row['destination'] == 'lib/shadow.py')
        self.assertEqual(shadow['sha256'], install.digest((self.sources / 'shadow.py').read_bytes()))
        self.assertFalse(self.prefix.exists())
        result = install.apply(plan)
        hook = json.loads((self.prefix / 'config/hooks.json').read_text())
        self.assertEqual(hook, {
            'schema': 'mneme.codex-hooks.config.v3',
            'project_root': str(self.project),
            'state_dir': str(self.project / '.mneme/codex-hook-state'),
            'service_config': str(self.prefix / 'config/service.json'),
            'memory_mode': 'shadow',
            'shadow_model': 'gpt-5.6-sol',
            'shadow_codex': str((self.sources / 'codex').resolve()),
            'shadow_codex_sha256': plan['shadow_codex']['sha256'],
        })
        codex_hooks = json.loads((self.project / '.codex/hooks.json').read_text())['hooks']
        self.assertEqual(codex_hooks['UserPromptSubmit'][-1]['hooks'][0]['additionalContextLimit'], 0)
        self.assertEqual(codex_hooks['SessionStart'][-1]['hooks'][0]['additionalContextLimit'], 1600)
        self.assertFalse((self.prefix / 'bin/codex-shadow').exists())
        for row in plan['files']:
            self.assertEqual(install.digest((self.prefix / row['destination']).read_bytes()), row['sha256'])
        self.assertEqual(json.loads((self.prefix / 'receipt.json').read_text())['plan'], plan)
        self.assertEqual(install.uninstall(result['receipt'])['status'], 'configuration removed')

    def test_shadow_source_drift_or_invalid_plan_refuses_before_install(self):
        plan = self.shadow_plan()
        changed = copy.deepcopy(plan)
        changed['shadow_model'] = 'not-a-model'
        changed['plan_sha256'] = install.plan_hash(changed)
        with self.assertRaisesRegex(ValueError, 'explicit supported shadow model'):
            install.apply(changed)
        (self.sources / 'codex').write_text('# changed fixture\n')
        with self.assertRaisesRegex(ValueError, 'shadow Codex executable changed'):
            install.apply(plan)
        self.assertFalse(self.prefix.exists())
        plan = self.shadow_plan()
        (self.sources / 'shadow.py').write_text('# changed helper\n')
        with self.assertRaisesRegex(ValueError, 'installation source changed'):
            install.apply(plan)
        self.assertFalse(self.prefix.exists())

    def test_effort_default_explicit_values_and_current_schema_are_reviewed(self):
        plan = self.async_plan()
        self.assertEqual(plan['librarian_effort'], 'medium')
        self.assertEqual(plan['reader_model'], 'gpt-6.1-sol')
        self.assertIn('lib/target_policy.py', [row['destination'] for row in plan['files']])
        self.assertEqual(plan['files'][-1]['destination'], 'lib/stewardship_contract.py')
        for effort in ('low', 'medium', 'high'):
            altered = copy.deepcopy(plan); altered['librarian_effort'] = effort
            altered['plan_sha256'] = install.plan_hash(altered)
            install.validate_plan(altered)
        for effort in (False, True, 1, [], {}, 'MEDIUM', 'xhigh'):
            altered = copy.deepcopy(plan); altered['librarian_effort'] = effort
            altered['plan_sha256'] = install.plan_hash(altered)
            with self.assertRaisesRegex(ValueError, 'librarian_effort'):
                install.validate_plan(altered)
        altered = copy.deepcopy(plan); del altered['librarian_effort']
        altered['plan_sha256'] = install.plan_hash(altered)
        with self.assertRaisesRegex(ValueError, 'effort is missing'):
            install.validate_plan(altered)
        with self.assertRaisesRegex(ValueError, 'require --recall-mode async'):
            install.prepare(self.project,self.prefix,self.sources/'mnemed',self.sources/'mneme-mcp',18765,
                            program_root=self.sources,librarian_effort='high')
        self.assertFalse(self.prefix.exists())

    def test_revision14_plan_stays_readable_but_cannot_install_old_view_contract(self):
        plan = self.async_plan(recording_mode='automatic')
        old = copy.deepcopy(plan)
        old['config_revision'] = 14
        old.pop("tag_stewardship", None)
        old.pop("tag_guide_id", None)
        old['files'] = old['files'][:len(install.V14_DESTINATIONS)]
        old['plan_sha256'] = install.plan_hash(old)
        install.validate_plan(old)
        original = {'config.toml': None, 'hooks.json': None}
        self.assertEqual(install.config_outputs(old, original),
                         install.config_outputs({**plan, 'config_revision': 17}, original))
        with self.assertRaisesRegex(ValueError, 'prepare a new plan'):
            install.apply(old)
        self.assertFalse(self.prefix.exists())

    def test_revision13_recovery_output_is_byte_stable_and_cannot_gain_new_policy(self):
        plan = self.async_plan(recording_mode='automatic')
        current = install.config_outputs({**plan, 'config_revision':17}, {'config.toml':None,'hooks.json':None})
        old = copy.deepcopy(plan); old['config_revision'] = 13; self.historic(old)
        old['plan_sha256'] = install.plan_hash(old)
        install.validate_plan(old)
        self.assertEqual(install.config_outputs(old, {'config.toml':None,'hooks.json':None}),current)
        self.assertEqual(old['reader_model'],'gpt-5.6-sol')
        self.assertEqual(len(old['files']),len(install.V13_DESTINATIONS))
        with self.assertRaisesRegex(ValueError,'prepare a new plan'):install.apply(old)
        injected = copy.deepcopy(old); injected['librarian_effort']='high'
        injected['plan_sha256']=install.plan_hash(injected)
        with self.assertRaisesRegex(ValueError,'old install plan cannot contain librarian effort'):install.validate_plan(injected)
        injected=copy.deepcopy(old);injected['reader_model']='gpt-6.1-sol'
        injected['plan_sha256']=install.plan_hash(injected)
        with self.assertRaisesRegex(ValueError,'supported reader model'):install.validate_plan(injected)
        self.assertFalse(self.prefix.exists())

    def test_async_requires_explicit_model_and_absolute_codex(self):
        for options, message in [
            ({'recall_mode': 'async'}, '--reader-model'),
            ({'recall_mode': 'async', 'reader_model': 'gpt-6.1-sol'}, 'absolute --reader-codex'),
            ({'reader_model': 'gpt-6.1-sol'}, 'require --recall-mode async'),
            ({'recall_mode': 'async', 'reader_model': 'gpt-5.6-terra'}, '--reader-model'),
            ({'recall_mode': 'async', 'reader_model': 'gpt-6.1-sol',
              'reader_codex': Path('codex')}, 'absolute --reader-codex'),
        ]:
            with self.subTest(options=options), self.assertRaisesRegex(ValueError, message):
                install.prepare(self.project, self.prefix, self.sources / 'mnemed',
                                self.sources / 'mneme-mcp', 18765, program_root=self.sources,
                                **options)
        self.assertFalse(self.prefix.exists())

    def test_async_bundle_hook_groups_and_uninstall(self):
        plan = self.async_plan()
        self.assertEqual(plan['reader_codex'], {
            'source': str((self.sources / 'codex').resolve()),
            'sha256': install.digest((self.sources / 'codex').read_bytes()),
        })
        self.assertEqual([row['destination'] for row in plan['files'][11:14]],
                         ['lib/reader_worker.py', 'lib/reader_runtime.py', 'lib/reader_contract.py'])
        result = install.apply(plan)
        hook = json.loads((self.prefix / 'config/hooks.json').read_text())
        self.assertEqual(hook, {
            'schema': 'mneme.codex-hooks.config.v8',
            'project_root': str(self.project),
            'state_dir': str(self.project / '.mneme/codex-hook-state'),
            'service_config': str(self.prefix / 'config/service.json'),
            'memory_mode': 'async',
            'reader_model': 'gpt-6.1-sol',
            'librarian_effort': 'medium', 'recording_mode': 'off',
            'tag_stewardship': False, 'tag_guide_id': None,
            'reader_codex': str((self.sources / 'codex').resolve()),
            'reader_codex_sha256': plan['reader_codex']['sha256'],
        })
        events = json.loads((self.project / '.codex/hooks.json').read_text())['hooks']
        self.assertEqual(set(events), {'SessionStart', 'UserPromptSubmit', 'Stop',
                                      'PostToolUse', 'Interrupt', 'SessionEnd'})
        self.assertEqual(len(events['UserPromptSubmit']), 2)
        sync_prompt = events['UserPromptSubmit'][0]['hooks'][0]
        background = events['UserPromptSubmit'][1]['hooks'][0]
        self.assertEqual(sync_prompt['additionalContextLimit'], 0)
        self.assertNotIn('async', sync_prompt)
        self.assertEqual(background, {
            'type': 'command', 'command': sync_prompt['command'] + ' --reader-background',
            'timeout': 5, 'async': True,
        })
        self.assertNotIn('additionalContextLimit', background)
        self.assertEqual(events['PostToolUse'][0]['hooks'][0]['additionalContextLimit'], 0)
        self.assertEqual(events['Interrupt'][0]['hooks'][0]['timeout'], 3)
        self.assertEqual(events['SessionEnd'][0]['hooks'][0]['timeout'], 3)
        self.assertEqual(len(events['SessionEnd']), 1)
        self.assertNotIn('async', events['SessionEnd'][0]['hooks'][0])
        for name in ('SessionStart', 'UserPromptSubmit', 'Stop', 'PostToolUse'):
            self.assertEqual(events[name][0]['hooks'][0]['timeout'], 5)
        self.assertEqual(install.uninstall(result['receipt'])['status'], 'configuration removed')

    def test_async_reader_pin_drift_and_plan_validation(self):
        plan = self.async_plan()
        altered = copy.deepcopy(plan)
        altered['reader_model'] = 'gpt-5.6-terra'
        altered['plan_sha256'] = install.plan_hash(altered)
        with self.assertRaisesRegex(ValueError, 'supported reader model'):
            install.apply(altered)
        (self.sources / 'codex').write_text('# changed fixture\n')
        with self.assertRaisesRegex(ValueError, 'reader Codex executable changed'):
            install.apply(plan)
        self.assertFalse(self.prefix.exists())

    def test_revision17_retains_exact_session_end_outputs_and_uninstall(self):
        plan = self.async_plan(recording_mode='automatic')
        result = install.apply(plan)
        original = {'config.toml': None, 'hooks.json': None}
        current = install.config_outputs(plan, original)
        expected = json.loads(current['hooks.json'])
        end = expected['hooks']['SessionEnd']
        end[0]['hooks'][0]['timeout'] = 5
        command = end[0]['hooks'][0]['command']
        end.append({'hooks': [{'type': 'command', 'command': command + ' --reader-background',
                               'timeout': 5, 'async': True}]})
        plan['config_revision'] = 17
        plan.pop("tag_stewardship", None)
        plan.pop("tag_guide_id", None)
        plan['files'] = plan['files'][:len(install.V19_DESTINATIONS)]
        plan['plan_sha256'] = install.plan_hash(plan)
        prior = install.config_outputs(plan, original)
        self.assertEqual(prior, {'config.toml': current['config.toml'],
                                 'hooks.json': install.json_bytes(expected)})
        for name, data in prior.items():
            (self.project / '.codex' / name).write_bytes(data)
        receipt_path = Path(result['receipt'])
        receipt = json.loads(receipt_path.read_text())
        receipt.update(plan=plan, installed_configs=install._output_hashes(prior))
        receipt_path.write_text(json.dumps(receipt))
        self.assertEqual(install.uninstall(receipt_path)['status'], 'configuration removed')

    def test_automatic_recording_installs_complete_bundle_and_close_wakes(self):
        plan = self.async_plan(recording_mode='automatic')
        self.assertEqual(plan['recording_mode'], 'automatic')
        names = ('turn_observer.py', 'rollout_primitives.py',
                 'recording_contract.py', 'recording_jobs.py', 'routing_memory.py', 'routing_contract.py', 'librarian_policy.py', 'touchstone_contract.py', 'target_policy.py',
                 'misc_binding.py', 'misc_config.py', 'hook_launcher.py', 'tag_config.py',
                 'tag_context.py', 'stewardship.py', 'stewardship_contract.py')
        self.assertEqual([row['destination'] for row in plan['files'][-len(names):]],
                         ['lib/' + name for name in names])
        result = install.apply(plan)
        hook = json.loads((self.prefix / 'config/hooks.json').read_text())
        self.assertEqual(hook['schema'], 'mneme.codex-hooks.config.v8')
        self.assertEqual(hook['recording_mode'], 'automatic')
        self.assertEqual(hook['memory_mode'], 'async')
        self.assertEqual(hook['project_root'], str(self.project))
        events = json.loads((self.project / '.codex/hooks.json').read_text())['hooks']
        for name in ('UserPromptSubmit', 'Stop'):
            self.assertEqual(len(events[name]), 2)
            front, back = [group['hooks'][0] for group in events[name]]
            self.assertNotIn('async', front)
            self.assertEqual(back, {'type': 'command',
                                   'command': front['command'] + ' --reader-background',
                                   'timeout': 5, 'async': True})
        for name in ('SessionStart', 'PostToolUse', 'Interrupt', 'SessionEnd'):
            self.assertEqual(len(events[name]), 1)
        for name in names:
            self.assertEqual((self.prefix / 'lib' / name).read_bytes(),
                             (self.sources / name).read_bytes())
        self.assertEqual(result['service'], 'not started')
        self.assertFalse((self.project / '.mneme').exists())
        self.assertEqual(install.uninstall(result['receipt'])['status'], 'configuration removed')

    def test_recording_rejects_invalid_modes_and_non_async_combinations(self):
        for mode in (None, True, 1, [], {}, 'enabled'):
            with self.subTest(mode=mode), self.assertRaisesRegex(ValueError, 'recording mode'):
                install.prepare(self.project, self.prefix, self.sources / 'mnemed',
                                self.sources / 'mneme-mcp', 18765, program_root=self.sources,
                                recording_mode=mode)
        for recall in ('reminder', 'automatic', 'shadow'):
            with self.subTest(recall=recall), self.assertRaisesRegex(ValueError, 'requires --recall-mode async'):
                install.prepare(self.project, self.prefix, self.sources / 'mnemed',
                                self.sources / 'mneme-mcp', 18765, program_root=self.sources,
                                recall_mode=recall, recording_mode='automatic')
        self.assertFalse(self.prefix.exists())
        self.assertFalse((self.project / '.codex').exists())

    def test_recording_plan_validation_before_apply(self):
        original = self.plan()
        for mode in (None, True, [], 'enabled', 'automatic'):
            altered = copy.deepcopy(original)
            altered['recording_mode'] = mode
            altered['plan_sha256'] = install.plan_hash(altered)
            with self.subTest(mode=mode), self.assertRaisesRegex(ValueError, 'recording'):
                install.apply(altered)
        del original['recording_mode']
        original['plan_sha256'] = install.plan_hash(original)
        with self.assertRaisesRegex(ValueError, 'recording mode'):
            install.apply(original)
        self.assertFalse(self.prefix.exists())

    def test_recording_module_drift_refuses_before_install(self):
        plan = self.async_plan(recording_mode='automatic')
        (self.sources / 'recording_jobs.py').write_text('# changed fixture\n')
        with self.assertRaisesRegex(ValueError, 'installation source changed'):
            install.apply(plan)
        self.assertFalse(self.prefix.exists())

    def test_revision9_retains_async_output_and_cannot_gain_recording(self):
        plan = self.async_plan(recording_mode='automatic')
        result = install.apply(plan)
        plan['config_revision'] = 9
        plan.pop("tag_stewardship", None)
        plan.pop("tag_guide_id", None)
        self.historic(plan)
        plan['files'] = plan['files'][:len(install.V9_DESTINATIONS)]
        plan['plan_sha256'] = install.plan_hash(plan)
        with self.assertRaisesRegex(ValueError, 'old install plan cannot contain recording mode'):
            install.validate_plan(plan)
        plan.pop('recording_mode')
        plan['plan_sha256'] = install.plan_hash(plan)
        install.validate_plan(plan)
        output = install.config_outputs(plan, {'config.toml': None, 'hooks.json': None})
        events = json.loads(output['hooks.json'])['hooks']
        self.assertEqual(len(events['UserPromptSubmit']), 2)
        self.assertEqual(len(events['Stop']), 1)
        self.assertEqual(len(events['SessionEnd']), 1)
        with self.assertRaisesRegex(ValueError, 'prepare a new plan'):
            install.apply(plan)
        for name, data in output.items():
            (self.project / '.codex' / name).write_bytes(data)
        receipt_path = Path(result['receipt'])
        receipt = json.loads(receipt_path.read_text())
        receipt['plan'] = plan
        receipt['installed_configs'] = install._output_hashes(output)
        receipt_path.write_text(json.dumps(receipt))
        self.assertEqual(install.uninstall(receipt_path)['status'], 'configuration removed')

    def test_revision10_receipt_uninstalls_without_routing_codec_or_new_plan(self):
        plan = self.async_plan(recording_mode='automatic')
        result = install.apply(plan)
        plan['config_revision'] = 10
        plan.pop("tag_stewardship", None)
        plan.pop("tag_guide_id", None)
        self.historic(plan)
        plan['files'] = plan['files'][:len(install.V10_DESTINATIONS)]
        plan['plan_sha256'] = install.plan_hash(plan)
        install.validate_plan(plan)
        self.assertNotIn('lib/routing_memory.py', [item['destination'] for item in plan['files']])
        with self.assertRaisesRegex(ValueError, 'prepare a new plan'):
            install.apply(plan)
        output = install.config_outputs(plan, {'config.toml': None, 'hooks.json': None})
        for name, data in output.items():
            (self.project / '.codex' / name).write_bytes(data)
        receipt_path = Path(result['receipt'])
        receipt = json.loads(receipt_path.read_text())
        receipt['plan'] = plan
        receipt['installed_configs'] = install._output_hashes(output)
        receipt_path.write_text(json.dumps(receipt))
        self.assertEqual(install.uninstall(receipt_path)['status'], 'configuration removed')

    def test_revision9_pending_recovers_original_async_output(self):
        plan = self.async_plan()
        plan['config_revision'] = 9
        plan.pop("tag_stewardship", None)
        plan.pop("tag_guide_id", None)
        self.historic(plan)
        plan['files'] = plan['files'][:len(install.V9_DESTINATIONS)]
        plan.pop('recording_mode')
        plan['plan_sha256'] = install.plan_hash(plan)
        output = install.config_outputs(plan, {'config.toml': None, 'hooks.json': None})
        config_dir = self.project / '.codex'
        config_dir.mkdir()
        for name, data in output.items():
            (config_dir / name).write_bytes(data)
        self.prefix.mkdir()
        pending = self.prefix / 'pending.json'
        pending.write_text(json.dumps(plan))
        self.assertEqual(install.recover(pending)['status'], 'aborted')
        self.assertFalse((config_dir / 'hooks.json').exists())

    def test_revision11_outputs_keep_exact_pre_save_authorization(self):
        plan = self.async_plan(recording_mode='automatic')
        before = {'config.toml': b'# existing config\n', 'hooks.json': None}
        current = install.config_outputs({**plan, 'config_revision':17}, before)
        old = copy.deepcopy(plan)
        old['config_revision'] = 11
        old.pop("tag_stewardship", None)
        old.pop("tag_guide_id", None)
        self.historic(old)
        old['plan_sha256'] = install.plan_hash(old)
        output = install.config_outputs(old, before)
        expected = current['config.toml'].replace(
            ('enabled_tools = ' + json.dumps(install.TOOLS)).encode(),
            ('enabled_tools = ' + json.dumps(install.EPISODE_TOOLS)).encode()).replace(
            f'[mcp_servers.{install.SERVER}.tools.save]\napproval_mode = "approve"\n'.encode(), b'').replace(
            f'[mcp_servers.{install.SERVER}.tools.concern]\napproval_mode = "approve"\n'.encode(), b'')
        self.assertEqual(output, {**current, 'config.toml': expected})
        server = tomllib.loads(output['config.toml'].decode())['mcp_servers'][install.SERVER]
        self.assertEqual(server['enabled_tools'], install.EPISODE_TOOLS)
        self.assertEqual(set(server['tools']), set(install.EPISODE_TOOLS))
        self.assertNotIn('save', server['tools'])
        with self.assertRaisesRegex(ValueError, 'old install plan'):
            install.apply(old)

    def test_revision12_keeps_exact_pre_concern_authorization(self):
        plan = self.async_plan(recording_mode='automatic')
        before = {'config.toml': b'# existing config\n', 'hooks.json': None}
        current = install.config_outputs({**plan, 'config_revision':17}, before)
        old = copy.deepcopy(plan)
        old['config_revision'] = 12
        old.pop("tag_stewardship", None)
        old.pop("tag_guide_id", None)
        self.historic(old)
        old['plan_sha256'] = install.plan_hash(old)
        output = install.config_outputs(old, before)
        expected = current['config.toml'].replace(
            ('enabled_tools = ' + json.dumps(install.TOOLS)).encode(),
            ('enabled_tools = ' + json.dumps(install.SAVE_TOOLS)).encode()).replace(
            f'[mcp_servers.{install.SERVER}.tools.concern]\napproval_mode = "approve"\n'.encode(), b'')
        self.assertEqual(output, {**current, 'config.toml': expected})
        server = tomllib.loads(output['config.toml'].decode())['mcp_servers'][install.SERVER]
        self.assertEqual(server['enabled_tools'], install.SAVE_TOOLS)
        self.assertEqual(set(server['tools']), set(install.SAVE_TOOLS))
        self.assertNotIn('concern', server['tools'])
        fresh = tomllib.loads(current['config.toml'].decode())['mcp_servers'][install.SERVER]
        self.assertIn('concern', fresh['enabled_tools'])
        self.assertEqual(fresh['tools']['concern']['approval_mode'], 'approve')

    def test_revision11_receipt_uninstalls_original_bytes_without_save(self):
        plan = self.async_plan(recording_mode='automatic')
        result = install.apply(plan)
        plan['config_revision'] = 11
        plan.pop("tag_stewardship", None)
        plan.pop("tag_guide_id", None)
        self.historic(plan)
        plan['plan_sha256'] = install.plan_hash(plan)
        output = install.config_outputs(plan, {'config.toml': None, 'hooks.json': None})
        config_dir = self.project / '.codex'
        for name, data in output.items():
            (config_dir / name).write_bytes(data)
        path = Path(result['receipt'])
        receipt = json.loads(path.read_text())
        receipt['plan'] = plan
        receipt['installed_configs'] = install._output_hashes(output)
        path.write_text(json.dumps(receipt))
        self.assertEqual(install.config_outputs(receipt['plan'], {'config.toml': None, 'hooks.json': None}), output)
        self.assertEqual(install.uninstall(path)['status'], 'configuration removed')

    def test_revision11_pending_recovers_original_bytes_without_save(self):
        plan = self.async_plan(recording_mode='automatic')
        plan['config_revision'] = 11
        plan.pop("tag_stewardship", None)
        plan.pop("tag_guide_id", None)
        self.historic(plan)
        plan['plan_sha256'] = install.plan_hash(plan)
        output = install.config_outputs(plan, {'config.toml': None, 'hooks.json': None})
        config_dir = self.project / '.codex'
        config_dir.mkdir()
        for name, data in output.items():
            (config_dir / name).write_bytes(data)
        self.prefix.mkdir()
        pending = self.prefix / 'pending.json'
        pending.write_text(json.dumps(plan))
        self.assertNotIn(b'.tools.save]', output['config.toml'])
        self.assertEqual(install.recover(pending)['status'], 'aborted')
        self.assertFalse((config_dir / 'config.toml').exists())
        self.assertFalse((config_dir / 'hooks.json').exists())

    def test_apply_preserves_other_hooks_and_config_uninstall_restores_exactly(self):
        config_dir = self.project / '.codex'
        config_dir.mkdir()
        original = (b'# existing comment\nmodel = "unchanged"\n'
                    b'approval_policy = "on-request"\nsandbox_mode = "workspace-write"\n'
                    b'[mcp_servers.other]\ncommand = "untouched"\n')
        old_hooks = b'{"description":"existing","hooks":{"Stop":[{"hooks":[{"type":"command","command":"echo keep"}]}]}}'
        (config_dir / 'config.toml').write_bytes(original)
        (config_dir / 'hooks.json').write_bytes(old_hooks)
        result = install.apply(self.plan())
        settings = tomllib.loads((config_dir / 'config.toml').read_text())
        self.assertEqual(settings['model'], 'unchanged')
        self.assertEqual(settings['approval_policy'], 'on-request')
        self.assertEqual(settings['sandbox_mode'], 'workspace-write')
        self.assertEqual(settings['mcp_servers']['other'], {'command': 'untouched'})
        server = settings['mcp_servers']['mneme_project']
        self.assertEqual(server['enabled_tools'], install.TOOLS)
        self.assertIn('episode', server['enabled_tools'])
        self.assertIn('save', server['enabled_tools'])
        self.assertEqual(server['command'], sys.executable)
        self.assertEqual(server['args'], [str(self.prefix / 'lib/launcher.py'),
                                          '--service-config', str(self.prefix / 'config/service.json')])
        self.assertEqual(server['startup_timeout_sec'], 30)
        self.assertNotIn('url', server)
        self.assertEqual(set(server['tools']), set(install.TOOLS))
        self.assertEqual(server['tools'], {name: {'approval_mode': 'approve'} for name in install.TOOLS})
        self.assertEqual(set(server), {'command', 'args', 'startup_timeout_sec', 'enabled_tools', 'tools'})
        hooks = json.loads((config_dir / 'hooks.json').read_text())
        self.assertEqual(len(hooks['hooks']['Stop']), 2)
        self.assertIn(str(self.prefix), hooks['hooks']['Stop'][1]['hooks'][0]['command'])
        for event in ('SessionStart', 'UserPromptSubmit'):
            self.assertEqual(hooks['hooks'][event][-1]['hooks'][0]['additionalContextLimit'], 1600)
        for event in ('SessionStart', 'UserPromptSubmit', 'Stop'):
            self.assertEqual(hooks['hooks'][event][-1]['hooks'][0]['timeout'], 5)
        self.assertNotIn('additionalContextLimit', hooks['hooks']['Stop'][-1]['hooks'][0])
        self.assertEqual(result['service'], 'not started')
        hook_config = json.loads((self.prefix / 'config/hooks.json').read_text())
        self.assertEqual(hook_config, {
            'schema': 'mneme.codex-hooks.config.v2',
            'project_root': str(self.project),
            'state_dir': str(self.project / '.mneme/codex-hook-state'),
            'service_config': str(self.prefix / 'config/service.json'),
            'recall_mode': 'reminder',
        })
        self.assertFalse((self.prefix / 'hook-state').exists())
        self.assertEqual(len(json.loads((self.prefix / 'receipt.json').read_text())['plan']['files']), len(install.DESTINATIONS))
        self.assertTrue((self.prefix / 'lib/launcher.py').exists())
        self.assertTrue((self.prefix / 'lib/hook_recall.py').exists())
        self.assertEqual((self.prefix / 'lib/shadow.py').read_bytes(), (self.sources / 'shadow.py').read_bytes())
        self.assertFalse((self.project / '.mneme').exists())
        install.uninstall(result['receipt'])
        self.assertEqual((config_dir / 'config.toml').read_bytes(), original)
        self.assertEqual((config_dir / 'hooks.json').read_bytes(), old_hooks)
        self.assertTrue(self.prefix.exists())

    def _prior_receipt(self, revision):
        original = self.plan()
        result = install.apply(original)
        config_dir = self.project / '.codex'
        originals = {'config.toml': None, 'hooks.json': None}
        old_plan = copy.deepcopy(original)
        old_plan['files'] = old_plan['files'][:6]
        old_plan.pop('shadow_model')
        old_plan.pop('shadow_codex')
        old_plan.pop('reader_model')
        old_plan.pop('reader_codex')
        old_plan.pop('recording_mode')
        old_plan.pop('recall_mode')
        if revision == 1:
            old_plan.pop('config_revision')
            self.historic(old_plan)
        else:
            old_plan['config_revision'] = revision
            old_plan.pop("tag_stewardship", None)
            old_plan.pop("tag_guide_id", None)
            self.historic(old_plan)
        old_plan['plan_sha256'] = install.plan_hash(old_plan)
        legacy = install.config_outputs(old_plan, originals)
        for name, data in legacy.items():
            (config_dir / name).write_bytes(data)
        receipt_path = Path(result['receipt'])
        receipt = json.loads(receipt_path.read_text())
        receipt['plan'] = old_plan
        receipt['installed_configs'] = install._output_hashes(legacy)
        receipt_path.write_text(json.dumps(receipt))
        return receipt_path, config_dir

    def test_prior_v3_receipt_can_uninstall_stdio_without_prompt_recall(self):
        original = self.plan()
        result = install.apply(original)
        old_plan = copy.deepcopy(original)
        old_plan['config_revision'] = 3
        old_plan.pop("tag_stewardship", None)
        old_plan.pop("tag_guide_id", None)
        self.historic(old_plan)
        old_plan.pop('recall_mode')
        old_plan['files'] = old_plan['files'][:7]
        old_plan.pop('shadow_model')
        old_plan.pop('shadow_codex')
        old_plan.pop('reader_model')
        old_plan.pop('reader_codex')
        old_plan.pop('recording_mode')
        old_plan['plan_sha256'] = install.plan_hash(old_plan)
        originals = {'config.toml': None, 'hooks.json': None}
        old_output = install.config_outputs(old_plan, originals)
        config_dir = self.project / '.codex'
        for name, data in old_output.items():
            (config_dir / name).write_bytes(data)
        receipt_path = Path(result['receipt'])
        receipt = json.loads(receipt_path.read_text())
        receipt['plan'] = old_plan
        receipt['installed_configs'] = install._output_hashes(old_output)
        receipt_path.write_text(json.dumps(receipt))
        self.assertEqual(install.uninstall(receipt_path)['status'], 'configuration removed')
        self.assertFalse((config_dir / 'config.toml').exists())
        self.assertFalse((config_dir / 'hooks.json').exists())

    def test_prior_v1_receipt_can_uninstall_without_new_approvals(self):
        receipt_path, config_dir = self._prior_receipt(1)
        self.assertEqual(install.uninstall(receipt_path)['status'], 'configuration removed')
        self.assertFalse((config_dir / 'config.toml').exists())
        self.assertFalse((config_dir / 'hooks.json').exists())

    def test_prior_v2_receipt_can_uninstall_http_approval_config(self):
        receipt_path, config_dir = self._prior_receipt(2)
        server = tomllib.loads((config_dir / 'config.toml').read_text())['mcp_servers']['mneme_project']
        self.assertEqual(server['url'], 'http://127.0.0.1:18765/')
        self.assertEqual(set(server['tools']), set(install.LEGACY_TOOLS))
        self.assertEqual(install.uninstall(receipt_path)['status'], 'configuration removed')
        self.assertFalse((config_dir / 'config.toml').exists())

    def test_prior_v2_pending_can_abort_its_http_output(self):
        plan = self.plan()
        result = install.apply(plan)
        old_plan = copy.deepcopy(plan)
        old_plan['config_revision'] = 2
        old_plan.pop("tag_stewardship", None)
        old_plan.pop("tag_guide_id", None)
        self.historic(old_plan)
        old_plan['files'] = old_plan['files'][:6]
        old_plan.pop('shadow_model')
        old_plan.pop('shadow_codex')
        old_plan.pop('reader_model')
        old_plan.pop('reader_codex')
        old_plan.pop('recording_mode')
        old_plan.pop('recall_mode')
        old_plan['plan_sha256'] = install.plan_hash(old_plan)
        originals = {'config.toml': None, 'hooks.json': None}
        old_output = install.config_outputs(old_plan, originals)
        for name, data in old_output.items():
            (self.project / '.codex' / name).write_bytes(data)
        Path(result['receipt']).unlink()
        pending = self.prefix / 'pending.json'
        pending.write_text(json.dumps(old_plan))
        self.assertEqual(install.recover(pending)['status'], 'aborted')
        self.assertFalse((self.project / '.codex/config.toml').exists())
        self.assertFalse((self.project / '.codex/hooks.json').exists())

    def test_prior_v3_pending_can_abort_its_stdio_output(self):
        plan = self.plan()
        result = install.apply(plan)
        old_plan = copy.deepcopy(plan)
        old_plan['config_revision'] = 3
        old_plan.pop("tag_stewardship", None)
        old_plan.pop("tag_guide_id", None)
        self.historic(old_plan)
        old_plan['files'] = old_plan['files'][:7]
        old_plan.pop('shadow_model')
        old_plan.pop('shadow_codex')
        old_plan.pop('reader_model')
        old_plan.pop('reader_codex')
        old_plan.pop('recording_mode')
        old_plan.pop('recall_mode')
        old_plan['plan_sha256'] = install.plan_hash(old_plan)
        old_output = install.config_outputs(old_plan, {'config.toml': None, 'hooks.json': None})
        for name, data in old_output.items():
            (self.project / '.codex' / name).write_bytes(data)
        Path(result['receipt']).unlink()
        pending = self.prefix / 'pending.json'
        pending.write_text(json.dumps(old_plan))
        self.assertEqual(install.recover(pending)['status'], 'aborted')
        self.assertFalse((self.project / '.codex/config.toml').exists())
        self.assertFalse((self.project / '.codex/hooks.json').exists())

    def test_old_prepared_plans_cannot_silently_change_transport(self):
        for revision in (1, 2, 3):
            with self.subTest(revision=revision):
                plan = self.plan()
                if revision == 1:
                    plan.pop('config_revision')
                else:
                    plan['config_revision'] = revision
                    plan.pop("tag_stewardship", None)
                    plan.pop("tag_guide_id", None)
                    self.historic(plan)
                plan['files'] = plan['files'][:7]
                plan.pop('shadow_model')
                plan.pop('shadow_codex')
                plan.pop('reader_model')
                plan.pop('reader_codex')
                plan.pop('recording_mode')
                plan.pop('recall_mode')
                if revision < 3:
                    plan['files'].pop()
                plan['plan_sha256'] = install.plan_hash(plan)
                with self.assertRaisesRegex(ValueError, 'prepare a new plan'):
                    install.apply(plan)
                plan['recall_mode'] = 'automatic'
                plan['plan_sha256'] = install.plan_hash(plan)
                with self.assertRaisesRegex(ValueError, 'old install plan cannot contain recall mode'):
                    install.apply(plan)
        self.assertFalse(self.prefix.exists())

    def test_changed_config_or_artifact_refuses_before_apply(self):
        plan = self.plan()
        config_dir = self.project / '.codex'
        config_dir.mkdir()
        (config_dir / 'config.toml').write_text('model = "new edit"\n')
        with self.assertRaisesRegex(ValueError, 'changed since prepare'):
            install.apply(plan)
        self.assertFalse(self.prefix.exists())
        (config_dir / 'config.toml').unlink()
        (self.sources / 'mnemed').write_text('changed')
        with self.assertRaisesRegex(ValueError, 'source changed'):
            install.apply(plan)
        self.assertFalse(self.prefix.exists())

    def test_changed_launcher_refuses_before_install(self):
        plan = self.plan()
        (self.sources / 'launcher.py').write_text('changed launcher fixture\n')
        with self.assertRaisesRegex(ValueError, 'source changed'):
            install.apply(plan)
        self.assertFalse(self.prefix.exists())

    def test_changed_recall_helper_refuses_before_install(self):
        plan = self.plan()
        (self.sources / 'hook_recall.py').write_text('changed helper fixture\n')
        with self.assertRaisesRegex(ValueError, 'source changed'):
            install.apply(plan)
        self.assertFalse(self.prefix.exists())

    def test_recall_mode_is_explicit_and_bound_to_reviewed_plan(self):
        with self.assertRaisesRegex(ValueError, 'recall mode'):
            install.prepare(self.project, self.prefix, self.sources / 'mnemed',
                            self.sources / 'mneme-mcp', 18765, recall_mode=None,
                            program_root=self.sources)
        with self.assertRaisesRegex(ValueError, 'recall mode'):
            install.prepare(self.project, self.prefix, self.sources / 'mnemed',
                            self.sources / 'mneme-mcp', 18765, recall_mode='all',
                            program_root=self.sources)
        plan = install.prepare(self.project, self.prefix, self.sources / 'mnemed',
                               self.sources / 'mneme-mcp', 18765, recall_mode='automatic',
                               program_root=self.sources)
        self.assertEqual(plan['recall_mode'], 'automatic')
        changed = copy.deepcopy(plan)
        changed['recall_mode'] = 'reminder'
        with self.assertRaisesRegex(ValueError, 'plan changed'):
            install.apply(changed)
        changed['plan_sha256'] = install.plan_hash(changed)
        changed['recall_mode'] = 1
        changed['plan_sha256'] = install.plan_hash(changed)
        with self.assertRaisesRegex(ValueError, 'planned recall mode'):
            install.apply(changed)
        result = install.apply(plan)
        hook = json.loads((self.prefix / 'config/hooks.json').read_text())
        self.assertEqual(hook['recall_mode'], 'automatic')
        install.uninstall(result['receipt'])

    def test_uninstall_preserves_subsequent_edits(self):
        result = install.apply(self.plan())
        config = self.project / '.codex/config.toml'
        config.write_text(config.read_text() + '\n# user edit\n')
        with self.assertRaisesRegex(ValueError, 'changed since install'):
            install.uninstall(result['receipt'])
        self.assertIn('user edit', config.read_text())
        self.assertTrue((self.project / '.codex/hooks.json').exists())

    def test_second_config_failure_restores_first_and_keeps_recovery_record(self):
        plan = self.plan()
        original_write = install.private_write
        def fail(path, data):
            if path == self.project / '.codex/hooks.json':
                raise OSError('fixture disk failure')
            original_write(path, data)
        with patch('install.private_write', side_effect=fail):
            with self.assertRaisesRegex(OSError, 'fixture disk failure'):
                install.apply(plan)
        self.assertFalse((self.project / '.codex/config.toml').exists())
        self.assertTrue((self.prefix / 'pending.json').exists())
        self.assertFalse((self.prefix / 'receipt.json').exists())
        self.assertEqual(install.recover(self.prefix / 'pending.json')['status'], 'aborted')
        self.assertFalse((self.prefix / 'pending.json').exists())

    def test_post_replace_failure_is_rolled_back_and_pending_can_be_aborted(self):
        plan = self.plan()
        original_write = install.private_write
        failed = False
        def fail_after_replace(path, data):
            nonlocal failed
            original_write(path, data)
            if path == self.project / '.codex/config.toml' and not failed:
                failed = True
                raise OSError('fixture directory fsync failure')
        with patch('install.private_write', side_effect=fail_after_replace):
            with self.assertRaisesRegex(OSError, 'directory fsync'):
                install.apply(plan)
        self.assertFalse((self.project / '.codex/config.toml').exists())
        self.assertFalse((self.project / '.codex/hooks.json').exists())
        self.assertTrue((self.prefix / 'pending.json').exists())
        self.assertEqual(install.recover(self.prefix / 'pending.json')['status'], 'aborted')

    def test_receipt_post_replace_failure_rolls_back_all_config(self):
        plan = self.plan()
        original_write = install.private_write
        def fail_after_receipt_replace(path, data):
            original_write(path, data)
            if path == self.prefix / 'receipt.json':
                raise OSError('fixture receipt directory fsync failure')
        with patch('install.private_write', side_effect=fail_after_receipt_replace):
            with self.assertRaisesRegex(OSError, 'receipt directory fsync'):
                install.apply(plan)
        self.assertFalse((self.project / '.codex/config.toml').exists())
        self.assertFalse((self.project / '.codex/hooks.json').exists())
        self.assertFalse((self.prefix / 'receipt.json').exists())
        self.assertEqual(install.recover(self.prefix / 'pending.json')['status'], 'aborted')

    def test_recover_finalizes_committed_receipt_with_leftover_pending(self):
        plan = self.plan()
        original_unlink = Path.unlink
        def fail_pending_unlink(path, *args, **kwargs):
            if path == self.prefix / 'pending.json':
                raise OSError('fixture pending cleanup failure')
            return original_unlink(path, *args, **kwargs)
        with patch.object(Path, 'unlink', fail_pending_unlink):
            result = install.apply(plan)
        self.assertEqual(result['status'], 'configured')
        self.assertEqual(result['pending_recovery'], str(self.prefix / 'pending.json'))
        self.assertTrue((self.prefix / 'pending.json').exists())
        self.assertEqual(install.recover(self.prefix / 'pending.json')['status'], 'configured')
        self.assertFalse((self.prefix / 'pending.json').exists())

    def test_recover_resumes_partial_rollback_after_receipt_replacement(self):
        config_dir = self.project / '.codex'
        config_dir.mkdir()
        old_config = b'model = "retained"\n'
        old_hooks = b'{"hooks":{}}'
        (config_dir / 'config.toml').write_bytes(old_config)
        (config_dir / 'hooks.json').write_bytes(old_hooks)
        plan = self.plan()
        original_write = install.private_write
        def fail_receipt_then_rollback(path, data):
            if path == config_dir / 'hooks.json' and data == old_hooks:
                raise OSError('fixture rollback interrupted')
            original_write(path, data)
            if path == self.prefix / 'receipt.json':
                raise OSError('fixture receipt fsync failure')
        with patch('install.private_write', side_effect=fail_receipt_then_rollback):
            with self.assertRaisesRegex(OSError, 'rollback interrupted'):
                install.apply(plan)
        self.assertEqual((config_dir / 'config.toml').read_bytes(), old_config)
        self.assertNotEqual((config_dir / 'hooks.json').read_bytes(), old_hooks)
        self.assertTrue((self.prefix / 'receipt.json').exists())
        self.assertEqual(install.recover(self.prefix / 'pending.json')['status'], 'aborted')
        self.assertEqual((config_dir / 'hooks.json').read_bytes(), old_hooks)
        self.assertFalse((self.prefix / 'receipt.json').exists())

    def test_uninstall_can_retry_after_second_restore_fails(self):
        config_dir = self.project / '.codex'
        config_dir.mkdir()
        (config_dir / 'config.toml').write_text('model = "retained"\n')
        (config_dir / 'hooks.json').write_text('{"hooks":{}}')
        result = install.apply(self.plan())
        original_write = install.private_write
        def fail_second(path, data):
            if path == config_dir / 'hooks.json':
                raise OSError('fixture second restore failure')
            original_write(path, data)
        with patch('install.private_write', side_effect=fail_second):
            with self.assertRaisesRegex(OSError, 'second restore'):
                install.uninstall(result['receipt'])
        self.assertEqual((config_dir / 'config.toml').read_text(), 'model = "retained"\n')
        self.assertNotEqual((config_dir / 'hooks.json').read_text(), '{"hooks":{}}')
        self.assertEqual(install.uninstall(result['receipt'])['status'], 'configuration removed')
        self.assertEqual((config_dir / 'hooks.json').read_text(), '{"hooks":{}}')

    def test_uninstall_retry_after_second_post_replace_failure(self):
        config_dir = self.project / '.codex'
        config_dir.mkdir()
        (config_dir / 'config.toml').write_text('model = "retained"\n')
        (config_dir / 'hooks.json').write_text('{"hooks":{}}')
        result = install.apply(self.plan())
        original_write = install.private_write
        def fail_after_replace(path, data):
            original_write(path, data)
            if path == config_dir / 'hooks.json':
                raise OSError('fixture restore fsync failure')
        with patch('install.private_write', side_effect=fail_after_replace):
            with self.assertRaisesRegex(OSError, 'restore fsync'):
                install.uninstall(result['receipt'])
        self.assertEqual(install.uninstall(result['receipt'])['status'], 'configuration removed')

    def test_plan_change_or_inconsistent_derived_fields_are_rejected(self):
        original = self.plan()
        for field, value in [('url', 'http://127.0.0.1:18888/'),
                             ('project_db', str(self.base / 'wrong.db')),
                             ('python', '/usr/bin/false')]:
            with self.subTest(field=field):
                plan = copy.deepcopy(original)
                plan[field] = value
                with self.assertRaisesRegex(ValueError, 'plan changed'):
                    install.apply(plan)
        plan = copy.deepcopy(original)
        plan['url'] = 'http://127.0.0.1:18888/'
        plan['plan_sha256'] = install.plan_hash(plan)
        with self.assertRaisesRegex(ValueError, 'URL'):
            install.apply(plan)
        plan = copy.deepcopy(original)
        plan['files'].pop()
        plan['plan_sha256'] = install.plan_hash(plan)
        with self.assertRaisesRegex(ValueError, 'files are incomplete'):
            install.apply(plan)
        self.assertFalse(self.prefix.exists())

    def test_existing_registration_or_symlink_is_not_overwritten(self):
        config_dir = self.project / '.codex'
        config_dir.mkdir()
        (config_dir / 'config.toml').write_text('[mcp_servers.mneme_project]\nurl="existing"\n')
        with self.assertRaisesRegex(ValueError, 'already exists'):
            self.plan()
        (config_dir / 'config.toml').unlink()
        (config_dir / 'config.toml').symlink_to(self.sources / 'mnemed')
        with self.assertRaisesRegex(ValueError, 'symlink'):
            self.plan()


if __name__ == '__main__':
    unittest.main()
