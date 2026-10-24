"""Native init orchestration uses disposable metadata/stores and no model/service."""
import argparse
import copy
import hashlib
import json
import os
from pathlib import Path
import sys
import tempfile
from types import SimpleNamespace
import unittest
from unittest.mock import patch

if sys.version_info < (3, 11):
    raise unittest.SkipTest("Native setup tests require Python 3.11+ for TOML parsing")
import tomllib

import project_setup as setup

DB = "01ARZ3NDEKTSV4RRFFQ69G5FAV"
OTHER = "01ARZ3NDEKTSV4RRFFQ69G5FAW"


class NativeProjectSetupTests(unittest.TestCase):
    def setUp(self):
        self.temp = tempfile.TemporaryDirectory()
        self.addCleanup(self.temp.cleanup)
        self.base = Path(self.temp.name).resolve()
        self.root = self.base / "project"
        self.root.mkdir()
        self.bin = self.base / "bin"
        self.bin.mkdir()
        for name in ("mnemed", "mneme-mcp", "codex"):
            (self.bin / name).write_text("#!/bin/sh\nexit 1\n")
            (self.bin / name).chmod(0o700)
        self.home = self.base / "home"
        self.memory = self.base / "config/mneme/memory.toml"
        self.memory.parent.mkdir(parents=True)
        self.memory.write_text('schema="mneme.config.v1"\n')
        self.args = argparse.Namespace(root=self.root, mnemed=self.bin / "mnemed",
            mcp_binary=self.bin / "mneme-mcp", codex_binary=None,
            library_helper=Path(__file__).parent.parent / "library/library.py",
            library_config=None, store_registry=None, port=19871, no_recording=False,
            no_trust_hooks=False, json=True, python="python3", memory_config=self.memory)
        self.env = patch.dict(os.environ, {"HOME":str(self.home), "CODEX_HOME":str(self.home / ".codex"),
            "XDG_CONFIG_HOME":str(self.base / "config"), "XDG_DATA_HOME":str(self.base / "data"),
            "XDG_CACHE_HOME":str(self.base / "cache"), "PATH":str(self.bin)})
        self.env.start()
        self.addCleanup(self.env.stop)
        self.events = []
        self.profile = "default"
        self.allow = None
        self.discovered = []
        self.core = True
        self.record = True
        self.read_default = None
        self.fail_final = False
        self.final_deny = False
        self.selected = patch("store_registry.native_select", side_effect=self.select)
        self.selected.start()
        self.addCleanup(self.selected.stop)
        self.capture = patch.object(setup, "native", side_effect=self.capture_call).start()
        self.ready = patch.object(setup, "ensure_ready", return_value={"state":"ready"}).start()
        self.trust = patch.object(setup, "trust_project_hooks", side_effect=AssertionError("no new hook actor")).start()
        self.addCleanup(patch.stopall)

    def capture_call(self, binary, root, *args):
        if args[-1] == "init":
            Path(args[1]).write_bytes(b"new disposable store")
        return {"db_id": DB}

    def select(self, config, binary, cwd, *, expected_selection_digest=None, **options):
        dot = self.root / ".mneme"
        overlay = dot / "memory.toml"
        codex = self.root / ".codex/config.toml"
        sources = []
        declarations = tomllib.loads(self.memory.read_text()).get("stores", {})
        declarations = copy.deepcopy(declarations)
        for role, path in (("user", self.memory), ("project", overlay), ("codex-config", codex)):
            if path.exists():
                raw = path.read_bytes()
                sources.append({"role":role, "path":str(path), "sha256":hashlib.sha256(raw).hexdigest(), "bytes":len(raw)})
                if role == "project":
                    additions = tomllib.loads(raw.decode()).get("stores", {})
                    for name, value in additions.items():
                        if name in declarations:
                            raise ValueError("native overlay duplicates a local store")
                        declarations[name] = value
        digest = hashlib.sha256(json.dumps([sources, dot.exists()], sort_keys=True).encode()).hexdigest()
        self.events.append(("replay" if expected_selection_digest is not None else "acquire", overlay.exists(), codex.exists(), digest))
        if expected_selection_digest is not None and expected_selection_digest != digest:
            raise ValueError("native selection changed")
        if self.fail_final and codex.exists():
            raise ValueError("final boundary selection unavailable")
        rows = copy.deepcopy(self.discovered)
        for name, store in declarations.items():
            eligible = {"read":True, "write":True, "core":self.core, "context":True,
                        "recall":self.record, "record":self.record, "maintenance":False}
            if self.final_deny and overlay.exists():
                eligible["read"] = False
            row = {"name":name, "store":store, "eligible":eligible, "active":True, "admitted":True, "reason":None}
            rows = [old for old in rows if old["store"]["id"] != store["id"]]
            rows.append(row)
        value = {"schema":"mneme.config-selection.v1", "digest":digest, "sources":sources,
            "cwd":str(self.root), "profile":self.profile,
            "project":{"configured":dot.exists(), "root":str(self.root) if dot.exists() else None,
                       "legacy_library":None, "legacy_allow_stores":None},
            "config":{"background":{"enabled":self.record, "model":"gpt-6.1-sol", "effort":"medium"},
                      "workspace":{"allow_stores":self.allow}},
            "stores":rows, "active":copy.deepcopy(rows),
            "read_default":self.read_default or (rows[-1]["name"] if rows else None)}
        return SimpleNamespace(snapshot=value)

    def run_setup(self):
        result = {"status":"incomplete", "stage":"preflight"}
        setup.setup(self.args, result)
        return result

    def shared_mcp(self, name="mneme"):
        path = self.home / ".codex/config.toml"
        path.parent.mkdir(parents=True, exist_ok=True)
        path.write_text(f'[mcp_servers.{json.dumps(name)}]\ncommand={json.dumps(str(self.bin / "mneme-mcp"))}\n'
            f'args={json.dumps(["--memory-config", str(self.memory)])}\n'
            'enabled_tools=["core","query","library_query"]\nstartup_timeout_sec=43\n'
            '[mcp_servers.' + json.dumps(name) + '.env]\nPRIVATE_MARKER="not-to-be-copied"\n'
            '[mcp_servers.' + json.dumps(name) + '.tools.query]\napproval_mode="approve"\n')
        return path

    def test_native_default_adopts_populated_data_without_legacy_policy_or_hooks(self):
        dot = self.root / ".mneme"
        dot.mkdir()
        (dot / "codex-memory.db").write_bytes(b"retained populated store")
        bodies = dot / "codex-memory.db.bodies"
        bodies.mkdir()
        (bodies / "note").write_bytes(b"retained authored body")
        global_config = self.shared_mcp()
        global_before = global_config.read_bytes()
        result = self.run_setup()
        self.assertEqual(result["integration"], "native")
        self.assertEqual(result["store"], "adopted")
        self.assertEqual((dot / "codex-memory.db").read_bytes(), b"retained populated store")
        self.assertEqual((bodies / "note").read_bytes(), b"retained authored body")
        self.assertEqual(self.capture.call_count, 1)
        self.assertEqual(self.capture.call_args.args[-1], "inspect")
        self.assertFalse((dot / "cli.json").exists())
        self.assertFalse((dot / "profile.json").exists())
        self.assertFalse((self.base / "data/libraries").exists())
        hooks = json.loads((self.root / ".codex/hooks.json").read_bytes())
        self.assertEqual(hooks, {"hooks":{}})
        local = (self.root / ".codex/config.toml").read_text()
        table = tomllib.loads(local)["mcp_servers"]
        self.assertEqual(set(table), {"mneme"})
        self.assertEqual(set(table["mneme"]), {"command", "args"})
        self.assertIn("--memory-config", local)
        self.assertIn("--expected-db-id", local)
        self.assertNotIn("not-to-be-copied", local)
        self.assertEqual(global_config.read_bytes(), global_before)
        plan = json.loads(Path(result["receipt"]).read_bytes())["plan"]
        self.assertNotIn("not-to-be-copied", json.dumps(plan))
        self.assertEqual(plan["native_managed"]["mcp"]["name"], "mneme")
        self.assertTrue(plan["native_managed"]["mcp"]["inherited"])
        self.assertEqual(json.loads(Path(result["service_config"]).read_bytes())["database_id"], DB)
        final = [event for event in self.events if event[0] == "acquire" and event[1:3] == (True, True)]
        self.assertTrue(final, "final Codex boundary must acquire its own native receipt")
        self.assertEqual(result["selection_digest"], final[-1][3])

    def test_final_boundary_is_acquired_and_offline_guarded_before_ready(self):
        self.ready.side_effect = lambda *args, **kwargs: self.assertEqual(self.events[-1][0:3], ("replay", True, True))
        self.run_setup()
        acquired = {(overlay, codex, digest) for kind, overlay, codex, digest in self.events if kind == "acquire"}
        self.assertEqual(len(acquired), 3)

    def test_rerun_uses_same_native_owner_without_offline_open_or_reinstall(self):
        first = self.run_setup()
        self.capture.reset_mock()
        overlay = self.root / ".mneme/memory.toml"
        before = overlay.read_bytes()
        with patch.object(setup.install, "apply", side_effect=AssertionError("no reinstall")):
            second = self.run_setup()
        self.capture.assert_not_called()
        self.assertEqual(second["db_id"], first["db_id"])
        self.assertEqual(overlay.read_bytes(), before)

    def test_native_core_background_off_and_explicit_default_are_not_overridden(self):
        self.core = self.record = False
        self.read_default = "user-chosen-other-store"
        result = self.run_setup()
        self.assertEqual(result["recording"], "off (native policy)")
        self.assertEqual(self.memory.read_text(), 'schema="mneme.config.v1"\n')
        self.assertNotIn("read_default", (self.root / ".mneme/memory.toml").read_text())

    def test_dotted_shared_server_and_discovered_name_are_literal_keys(self):
        self.shared_mcp("mneme.shared")
        dot = self.root / ".mneme"
        dot.mkdir()
        (dot / "codex-memory.db").write_bytes(b"existing differently named project")
        self.allow = ["project.existing"]
        self.discovered = [{"name":"project.existing", "store":{"id":DB,
            "owner":{"url":"http://127.0.0.1:19999/", "db":"project",
                     "path":str(self.root / ".mneme/codex-memory.db")}},
            "active":True, "admitted":True, "eligible":{"read":True}, "reason":None}]
        self.run_setup()
        overlay = tomllib.loads((self.root / ".mneme/memory.toml").read_text())
        self.assertEqual(set(overlay["stores"]), {"project.existing"})
        local = tomllib.loads((self.root / ".codex/config.toml").read_text())
        self.assertEqual(set(local["mcp_servers"]), {"mneme.shared"})

    def test_native_known_denial_refuses_before_store_creation(self):
        for profile, allow in (("private", None), ("default", [])):
            with self.subTest(profile=profile, allow=allow):
                self.profile, self.allow = profile, allow
                with self.assertRaisesRegex(ValueError, "excluded|does not admit"):
                    self.run_setup()
                self.capture.assert_not_called()
                self.ready.assert_not_called()
                self.assertFalse((self.root / ".mneme").exists())

    def test_known_local_name_collision_refuses_before_create(self):
        self.memory.write_text(f'schema="mneme.config.v1"\n[stores.project]\nid="{OTHER}"\n'
            'owner={url="http://127.0.0.1:19999/",db="project"}\n')
        with self.assertRaisesRegex(ValueError, "duplicate a local store name"):
            self.run_setup()
        self.capture.assert_not_called()
        self.assertFalse((self.root / ".mneme").exists())

    def test_final_id_denial_preserves_created_store_but_rolls_back_overlay(self):
        self.final_deny = True
        with self.assertRaisesRegex(ValueError, "read grant"):
            self.run_setup()
        self.assertEqual((self.root / ".mneme/codex-memory.db").read_bytes(), b"new disposable store")
        self.assertFalse((self.root / ".mneme/memory.toml").exists())
        self.assertFalse((self.root / ".codex").exists())
        self.ready.assert_not_called()

    def test_post_install_selection_failure_restores_exact_owned_config(self):
        self.fail_final = True
        codex = self.root / ".codex"
        codex.mkdir()
        original = b'model="existing-choice"\n'
        (codex / "config.toml").write_bytes(original)
        # Model failure is injected only once the generated managed entry exists.
        original_select = self.select
        def fail_generated(*args, **kwargs):
            current = (codex / "config.toml").read_text()
            self.fail_final = "--memory-config" in current
            return original_select(*args, **kwargs)
        with patch("store_registry.native_select", side_effect=fail_generated):
            with self.assertRaisesRegex(ValueError, "final boundary"):
                self.run_setup()
        self.assertEqual((codex / "config.toml").read_bytes(), original)
        self.assertFalse((codex / "hooks.json").exists())
        self.assertFalse((self.root / ".mneme/memory.toml").exists())
        self.assertTrue((self.root / ".mneme/codex-memory.db").exists())
        self.ready.assert_not_called()

    def test_readiness_failure_retains_exact_resumable_native_pair(self):
        self.ready.side_effect = RuntimeError("owner unavailable")
        with self.assertRaisesRegex(RuntimeError, "owner unavailable"):
            self.run_setup()
        self.assertTrue((self.root / ".mneme/codex-integration/receipt.json").exists())
        self.assertTrue((self.root / ".mneme/memory.toml").exists())
        self.capture.reset_mock()
        self.ready.side_effect = None
        self.run_setup()
        self.capture.assert_not_called()

    def test_local_identity_claim_is_not_overridden(self):
        dot = self.root / ".mneme"
        dot.mkdir()
        (dot / "codex-memory.db").write_bytes(b"retained")
        self.memory.write_text(f'schema="mneme.config.v1"\n[stores.explicit]\nid="{DB}"\n'
            'owner={url="http://127.0.0.1:19999/",db="project"}\n')
        with self.assertRaisesRegex(ValueError, "duplicate a local store name/identity"):
            self.run_setup()
        self.assertEqual((dot / "codex-memory.db").read_bytes(), b"retained")
        self.assertFalse((dot / "memory.toml").exists())
        self.ready.assert_not_called()

    def test_malformed_or_unresolved_native_selection_never_falls_back(self):
        with patch("store_registry.native_select", side_effect=ValueError("native configuration refused")):
            with self.assertRaisesRegex(ValueError, "native configuration refused"):
                self.run_setup()
        self.capture.assert_not_called()
        self.ready.assert_not_called()
        self.assertFalse((self.root / ".mneme").exists())
        self.assertFalse((self.root / ".codex").exists())

    def test_legacy_policy_selectors_are_preserved_not_migrated_implicitly(self):
        dot = self.root / ".mneme"
        dot.mkdir()
        profile = dot / "profile.json"
        original = setup.install.json_bytes({"schema":"mneme.profile.v1", "mode":"default",
            "library_config":str(self.base / "retained-library.json")})
        profile.write_bytes(original)
        with self.assertRaisesRegex(ValueError, "explicit migration"):
            self.run_setup()
        self.capture.assert_not_called()
        self.ready.assert_not_called()
        self.assertEqual(profile.read_bytes(), original)
        self.assertFalse((dot / "memory.toml").exists())

    def test_same_name_policy_changes_inherit_but_registration_reroute_refuses(self):
        global_path = self.shared_mcp()
        self.run_setup()
        self.capture.reset_mock()
        self.ready.reset_mock()
        raw = global_path.read_text()
        global_path.write_text(raw.replace('startup_timeout_sec=43', 'startup_timeout_sec=44'))
        self.run_setup()  # Non-launch policy remains user-owned and inherited.
        self.capture.assert_not_called()
        self.ready.reset_mock()
        global_path.write_text(raw.replace('[mcp_servers."mneme"', '[mcp_servers."renamed"'))
        with self.assertRaisesRegex(ValueError, "registration changed"):
            self.run_setup()
        self.capture.assert_not_called()
        self.ready.assert_not_called()

    def test_changed_service_identity_on_rerun_refuses_without_offline_open(self):
        result = self.run_setup()
        self.capture.reset_mock()
        self.ready.reset_mock()
        service_path = Path(result["service_config"])
        config = json.loads(service_path.read_bytes())
        config["database_id"] = OTHER
        service_path.write_bytes(setup.install.json_bytes(config))
        with self.assertRaisesRegex(ValueError, "service configuration changed"):
            self.run_setup()
        self.capture.assert_not_called()
        self.ready.assert_not_called()

    def test_concurrent_overlay_edit_is_not_replaced_by_rollback(self):
        foreign = b'schema="mneme.config.v1"\n# concurrent user edit\n'
        original = self.select
        def changed(*args, **kwargs):
            overlay = self.root / ".mneme/memory.toml"
            if overlay.exists():
                overlay.write_bytes(foreign)
                raise ValueError("native selection drift")
            return original(*args, **kwargs)
        with patch("store_registry.native_select", side_effect=changed):
            with self.assertRaisesRegex(ValueError, "changed concurrently"):
                self.run_setup()
        self.assertEqual((self.root / ".mneme/memory.toml").read_bytes(), foreign)
        self.ready.assert_not_called()

    def test_concurrent_unknown_install_prefix_is_preserved(self):
        prefix = self.root / ".mneme/codex-integration"
        original_prepare = setup.install.prepare
        def appeared(*args, **kwargs):
            prefix.mkdir()
            (prefix / "pending.json").write_text('{"foreign":"untouched"}')
            return original_prepare(*args, **kwargs)
        with patch.object(setup.install, "prepare", side_effect=appeared):
            with self.assertRaisesRegex(ValueError, "prefix must be absent"):
                self.run_setup()
        self.assertEqual((prefix / "pending.json").read_text(), '{"foreign":"untouched"}')
        self.assertFalse((self.root / ".mneme/memory.toml").exists())
        self.ready.assert_not_called()

    def test_fixed_workspace_inherited_server_is_not_silently_repaired(self):
        path = self.shared_mcp()
        original = path.read_text()
        for replacement in (
            original.replace('startup_timeout_sec=43', 'startup_timeout_sec=43\ncwd=' + json.dumps(str(self.root))),
            original.replace(json.dumps(["--memory-config", str(self.memory)]),
                             json.dumps(["--memory-config", str(self.memory), "--cwd", str(self.root)])),
        ):
            with self.subTest(config=replacement):
                path.write_text(replacement)
                with self.assertRaisesRegex(ValueError, "unsupported workspace"):
                    self.run_setup()
                self.capture.assert_not_called()
                self.ready.assert_not_called()
                self.assertFalse((self.root / ".mneme").exists())
                self.assertEqual(path.read_text(), replacement)

    def test_independent_actor_override_is_not_silently_ignored(self):
        self.args.codex_binary = self.bin / "codex"
        with self.assertRaisesRegex(ValueError, "codex-binary cannot override"):
            self.run_setup()
        self.capture.assert_not_called()
        self.ready.assert_not_called()
        self.assertFalse((self.root / ".mneme").exists())


if __name__ == "__main__":
    unittest.main()
