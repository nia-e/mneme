"""Symbolic reader configuration; no providers, native processes or live owners."""
import copy
import json
import os
from pathlib import Path
import tempfile
import unittest
from unittest.mock import patch

import hooks
import registry_runtime
import store_registry
from recording_jobs import _config_digest


class ReaderPathConfigTests(unittest.TestCase):
    def setUp(self):
        temporary = tempfile.TemporaryDirectory()
        self.addCleanup(temporary.cleanup)
        self.root = Path(temporary.name).resolve()
        self.path = self.root / "hooks.json"
        self.native = {"schema": registry_runtime.NATIVE_SCHEMA,
            "state_dir": str(self.root / "state"), "memory_config": str(self.root / "memory.toml"),
            "native_cli": str(self.root / "mnemed"), "reader_codex": "codex"}
        self.registry = {"schema": registry_runtime.SCHEMA,
            "state_dir": str(self.root / "state"), "store_registry": str(self.root / "stores.json"),
            "memory_scope": "registry", "memory_mode": "async", "recording_mode": "off",
            "reader_model": "gpt-6.1-sol", "librarian_effort": "medium", "reader_codex": "codex"}
        Path(self.registry["store_registry"]).write_text(json.dumps({"schema": "mneme.stores.v1", "stores": []}))
        self.pinned = {"reader_codex": str(self.root / "explicit-codex"), "reader_codex_sha256": "b" * 64}

    def native_selection(self, *_args, **_kwargs):
        snapshot = {"schema": "mneme.config-selection.v1", "digest": "a" * 64,
            "cwd": str(self.root), "config": {"background": {"model": "gpt-6.1-sol", "effort": "medium"}},
            "active": []}
        return store_registry.NativeSelection(snapshot, self.native["memory_config"],
                                               self.native["native_cli"], str(self.root))

    def load_native(self, value):
        self.path.write_text(json.dumps(value))
        with patch("store_registry.native_select", side_effect=self.native_selection), \
             patch("host_process.spawn") as spawn, patch("host_platform.shutil.which") as lookup:
            loaded = hooks._config(self.path, cwd=str(self.root))
            spawn.assert_not_called()
            lookup.assert_not_called()
        return loaded

    def test_native_and_v12_validate_modes_without_resolving_path_command(self):
        for value, validate in ((self.native, registry_runtime.validate_native_static),
                                (self.registry, registry_runtime.validate_static)):
            with self.subTest(schema=value["schema"]):
                self.assertEqual(validate(value), value)
                self.assertEqual(validate({**value, **self.pinned}), {**value, **self.pinned})
                for change in ({"reader_codex_sha256": None}, {"reader_codex_sha256": "b" * 64},
                               {"reader_codex": "./codex"}, {"reader_codex": "other"}):
                    with self.subTest(change=change), self.assertRaises(ValueError):
                        validate({**value, **change})

    def test_native_load_keeps_symbolic_reader_and_config_pin_across_executable_updates(self):
        command = self.root / "codex"
        command.write_bytes(b"first installed edition")
        with patch.dict(os.environ, {"PATH": str(self.root)}), \
             patch("store_registry.native_select", side_effect=self.native_selection), \
             patch("host_process.spawn") as spawn:
            loaded = self.load_native(self.native)
            original = copy.deepcopy(loaded)
            pin = loaded["_reader_config_pin"]
            command.write_bytes(b"second installed edition")
            self.assertEqual(_config_digest(loaded), pin)
            self.assertEqual(loaded, original)
            self.assertEqual(loaded["reader_codex"], "codex")
            self.assertNotIn("reader_codex_sha256", loaded)
            again = self.load_native(self.native)
            self.assertEqual(again["_reader_config_pin"], pin)
            spawn.assert_not_called()
        self.assertFalse((self.root / "state").exists())

    def test_absolute_unpinned_selector_is_lexical_and_does_not_require_executable(self):
        selector = str(self.root / 'symlink-codex')
        target = self.root / 'version-one'
        target.write_text('first')
        Path(selector).symlink_to(target)
        value = {**self.native, 'reader_codex': selector}
        loaded = self.load_native(value)
        self.assertEqual(str(loaded['reader_codex']), selector)
        self.assertNotIn('reader_codex_sha256', loaded)
        target.unlink()
        self.assertEqual(str(self.load_native(value)['reader_codex']), selector)

    def test_workspace_override_replaces_reader_mode_without_inherited_hash(self):
        for parent, child in ((self.pinned, {"reader_codex": "codex"}),
                              ({"reader_codex": "codex"}, self.pinned),
                              (self.pinned, {"reader_codex": self.pinned["reader_codex"]})):
            with self.subTest(parent=parent, child=child):
                value = {**self.native, **parent, "workspace_adapters": [
                    {"root": str(self.root), "state_dir": str(self.root / "workspace-state"), **child}]}
                original = copy.deepcopy(value)
                loaded = self.load_native(value)
                self.assertEqual(loaded["state_dir"], self.root / "workspace-state")
                expected = child["reader_codex"]
                self.assertEqual(loaded["reader_codex"], expected if expected == "codex" else Path(expected))
                self.assertEqual("reader_codex_sha256" in loaded, "reader_codex_sha256" in child)
                if "reader_codex_sha256" in child:
                    self.assertEqual(loaded["reader_codex_sha256"], child["reader_codex_sha256"])
                self.assertEqual(value, original)

    def test_workspace_path_mode_rejects_ambiguous_hash_and_relative_commands(self):
        base = {"root": str(self.root), "state_dir": str(self.root / "workspace-state"), "reader_codex": "codex"}
        for change in ({"reader_codex_sha256": None}, {"reader_codex_sha256": "b" * 64},
                       {"reader_codex": "elsewhere"}):
            with self.subTest(change=change), self.assertRaises(ValueError):
                registry_runtime.validate_native_static({**self.native, "workspace_adapters": [{**base, **change}]})

    def test_v12_load_keeps_symbolic_reader_without_discovery_or_state_creation(self):
        self.path.write_text(json.dumps(self.registry))
        binding = {"workspace_root": str(self.root), "workspace_origin": str(self.root)}
        with patch("host_platform.shutil.which") as lookup, patch("host_process.spawn") as spawn:
            loaded = hooks._config(self.path, cwd=str(self.root), workspace_binding=binding)
            self.assertEqual(loaded["reader_codex"], "codex")
            self.assertNotIn("reader_codex_sha256", loaded)
            self.assertEqual(loaded["_reader_config_pin"], _config_digest(loaded))
            lookup.assert_not_called()
            spawn.assert_not_called()
        self.assertFalse((self.root / "state").exists())


if __name__ == "__main__":
    unittest.main()
