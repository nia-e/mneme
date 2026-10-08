"""Filesystem-only misc authority selection, including privacy ancestors."""
import json
import os
from pathlib import Path
import tempfile
import unittest
from unittest.mock import patch

import misc_binding


class MiscBindingTests(unittest.TestCase):
    def setUp(self):
        temporary = tempfile.TemporaryDirectory()
        self.addCleanup(temporary.cleanup)
        self.base = Path(temporary.name).resolve()
        self.root = self.base / "project"
        self.root.mkdir()

    def codex(self, name, text, root=None):
        directory = (root or self.root) / ".codex"
        directory.mkdir(exist_ok=True)
        (directory / name).write_text(text)

    def test_plain_git_is_eligible_and_selection_has_no_service_contact(self):
        (self.root / ".git").mkdir()
        with patch("service.load_config", side_effect=AssertionError("must not load service")):
            binding = misc_binding.choose_workspace(self.root)
            self.assertEqual(binding, {"workspace_root": str(self.root), "workspace_origin": str(self.root)})
            self.assertEqual(misc_binding.validate_binding(binding), binding)

    def test_explicit_profiles_and_even_empty_owner_directories_are_not_misc(self):
        owner = self.root / ".mneme"
        owner.mkdir()
        self.assertIsNone(misc_binding.choose_workspace(self.root))
        for mode in ("default", "private", "isolated"):
            (owner / "profile.json").write_text(json.dumps({"schema": "mneme.profile.v1", "mode": mode}))
            self.assertIsNone(misc_binding.choose_workspace(self.root))
        (owner / "profile.json").write_text('{"schema":"unknown","mode":"default"}')
        with self.assertRaises(ValueError):
            misc_binding.choose_workspace(self.root)

    def test_privacy_ancestor_survives_nested_git_boundary(self):
        (self.base / ".mneme").mkdir()
        (self.base / ".mneme/profile.json").write_text('{"schema":"mneme.profile.v1","mode":"isolated"}')
        (self.root / ".git").mkdir()
        self.assertIsNone(misc_binding.choose_workspace(self.root))

    def test_exclusions_and_binding_drift(self):
        binding = misc_binding.choose_workspace(self.root)
        self.assertIsNone(misc_binding.choose_workspace(self.root, [str(self.base)]))
        with self.assertRaises(ValueError):
            misc_binding.validate_binding(binding, [str(self.base)])
        (self.root / ".mneme").mkdir()
        with self.assertRaisesRegex(ValueError, "configured or excluded"):
            misc_binding.validate_binding(binding)

    def test_unrelated_valid_codex_configs_do_not_disable_misc(self):
        self.codex("config.toml", 'model = "gpt-6.1-sol"\ndeveloper_instructions = "Work on mneme carefully"\n[mcp_servers.weather]\ncommand = "weather-cli"\n')
        self.codex("hooks.json", json.dumps({"hooks": {"Stop": [{"hooks": [
            {"type": "command", "command": "echo done"}]}]}}))
        self.assertIsNotNone(misc_binding.choose_workspace(self.root))

    def test_hook_ownership_uses_routes_not_status_or_state_metadata(self):
        self.codex("hooks.json", json.dumps({"hooks": {"Stop": [{"hooks": [
            {"type": "command", "command": "echo okay", "statusMessage": "Working on Mneme",
             "description": "Mneme is a project, not this handler"}]}]}}))
        self.codex("config.toml", '[hooks]\nenabled = true\n'
                   'Stop = [{hooks = [{type = "command", command = "echo okay", statusMessage = "Working on Mneme"}]}]\n'
                   '[hooks.state]\n"/old/mneme/hooks.json" = "trusted"\n')
        self.assertIsNotNone(misc_binding.choose_workspace(self.root))

    def test_mcp_hook_route_detects_owned_server_or_tool_not_metadata(self):
        for server, tool, eligible in (("mneme_project", "recall_context", False),
                                       ("weather", "weather", True),
                                       ("proxy", "mneme_recall", False)):
            value = {"hooks": {"Stop": [{"hooks": [{"type": "mcp_tool", "server": server,
                     "tool": tool, "statusMessage": "Working on Mneme"}]}]}}
            self.codex("hooks.json", json.dumps(value))
            with self.subTest(server=server, tool=tool):
                self.assertEqual(misc_binding.choose_workspace(self.root) is not None, eligible)
        (self.root / ".codex/hooks.json").unlink()
        self.codex("config.toml", '[hooks]\nStop = [{hooks = [{type = "mcp_tool", server = "mneme_project", tool = "recall_context"}]}]\n')
        self.assertIsNone(misc_binding.choose_workspace(self.root))

    def test_unknown_or_malformed_hook_routes_refuse(self):
        for handler in ({"type": "future", "command": "echo okay"},
                        {"type": "mcp_tool", "server": "weather"},
                        {"type": "mcp_tool", "server": 12, "tool": "weather"},
                        {"type": "prompt", "prompt": False}):
            self.codex("hooks.json", json.dumps({"hooks": {"Stop": [{"hooks": [handler]}]}}))
            with self.subTest(handler=handler), self.assertRaises(ValueError):
                misc_binding.choose_workspace(self.root)

    def test_managed_hooks_are_explicit_boundaries_without_directory_discovery(self):
        for field in ("managed_dir", "windows_managed_dir"):
            self.codex("config.toml", '[hooks]\n' + field + ' = "/unavailable/managed/hooks"\n')
            with self.subTest(field=field):
                self.assertIsNone(misc_binding.choose_workspace(self.root))

    def test_known_prompt_hooks_do_not_own_mneme_routing(self):
        self.codex("hooks.json", json.dumps({"hooks": {"Stop": [{"hooks": [
            {"type": "prompt", "prompt": "Work on Mneme carefully"},
            {"type": "agent", "prompt": "Work on Mneme carefully"}]}]}}))
        self.assertIsNotNone(misc_binding.choose_workspace(self.root))

    def test_owned_toml_and_hooks_including_legacy_are_not_misc(self):
        for text in ('[mcp_servers.mneme_project]\nurl = "http://127.0.0.1:12345"\n',
                     '[mcp_servers.other]\ncommand = "python3"\nargs = ["/old/copied/launcher.py"]\n'):
            self.codex("config.toml", text)
            self.assertIsNone(misc_binding.choose_workspace(self.root))
        (self.root / ".codex/config.toml").unlink()
        self.codex("hooks.json", json.dumps({"hooks": {"Stop": [{"hooks": [
            {"type": "command", "command": "python3 /copied/hooks.py --config /legacy/config.json"}]}]}}))
        self.assertIsNone(misc_binding.choose_workspace(self.root))
        self.codex("hooks.json", '{"schema":"mneme.codex-hooks.config.v4"}')
        self.assertIsNone(misc_binding.choose_workspace(self.root))

    def test_global_device_hooks_do_not_self_suppress_descendants(self):
        self.codex("config.toml", '[mcp_servers.mneme_project]\ncommand="mneme"\n', root=self.base)
        self.codex("hooks.json", '{"hooks":{"Stop":[{"hooks":[{"type":"command","command":"mneme"}]}]}}', root=self.base)
        with patch.dict(os.environ, {"CODEX_HOME": str(self.base / ".codex")}):
            self.assertIsNotNone(misc_binding.choose_workspace(self.root))
        self.assertIsNone(misc_binding.choose_workspace(self.root))

    def test_workspace_codex_symlink_to_global_directory_still_refuses(self):
        global_dir = self.base / "global" / "codex"
        global_dir.mkdir(parents=True)
        (global_dir / "hooks.json").write_text('{"schema":"mneme.codex-hooks.config.v4"}')
        (self.root / ".codex").symlink_to(global_dir, target_is_directory=True)
        with patch.dict(os.environ, {"CODEX_HOME": str(global_dir)}):
            with self.assertRaisesRegex(ValueError, "symlink"):
                misc_binding.choose_workspace(self.root)

    def test_real_global_codex_directory_ignores_legacy_hook_self(self):
        self.codex("hooks.json", '{"schema":"mneme.codex-hooks.config.v4"}', root=self.base)
        with patch.dict(os.environ, {"CODEX_HOME": str(self.base / ".codex")}):
            self.assertIsNotNone(misc_binding.choose_workspace(self.root))

    def test_invalid_unknown_oversized_or_symlinked_configs_fail_closed(self):
        self.codex("hooks.json", '{"hooks":{"Stop":"not-array"}}')
        with self.assertRaises(ValueError):
            misc_binding.choose_workspace(self.root)
        self.codex("hooks.json", '{"future":true}')
        with self.assertRaises(ValueError):
            misc_binding.choose_workspace(self.root)
        self.codex("hooks.json", ' ' * (misc_binding.MAX_CONFIG_BYTES + 1))
        with self.assertRaisesRegex(ValueError, "bounded"):
            misc_binding.choose_workspace(self.root)
        (self.root / ".codex/hooks.json").unlink()
        (self.root / ".codex/hooks.json").symlink_to(self.base / "absent")
        with self.assertRaisesRegex(ValueError, "symlink"):
            misc_binding.choose_workspace(self.root)

    def test_workspace_alias_retains_lexical_origin_and_canonical_root(self):
        alias = self.base / "alias"
        alias.symlink_to(self.root, target_is_directory=True)
        aliased = misc_binding.choose_workspace(alias)
        self.assertEqual(aliased, {"workspace_root": str(self.root), "workspace_origin": str(alias)})
        self.assertEqual(misc_binding.validate_binding(aliased), aliased)
        alias.unlink()
        alias.symlink_to(self.base, target_is_directory=True)
        with self.assertRaises(ValueError):
            misc_binding.validate_binding(aliased)
        binding = misc_binding.choose_workspace(self.root)
        with self.assertRaises(ValueError):
            misc_binding.validate_binding({**binding, "workspace_origin": str(self.base)})
        with self.assertRaises(ValueError):
            misc_binding.validate_binding({**binding, "workspace_root": str(self.root) + "/../project"})

    def test_lexical_and_canonical_privacy_ancestry_and_exclusions(self):
        private = self.base / "private"
        private.mkdir()
        alias = private / "alias"
        alias.symlink_to(self.root, target_is_directory=True)
        self.assertIsNotNone(misc_binding.choose_workspace(alias))
        self.assertIsNone(misc_binding.choose_workspace(alias, [str(private)]))
        self.assertIsNone(misc_binding.choose_workspace(alias, [str(self.root)]))
        (private / ".mneme").mkdir()
        self.assertIsNone(misc_binding.choose_workspace(alias))
        (private / ".mneme").rmdir()
        (self.root / ".mneme").mkdir()
        self.assertIsNone(misc_binding.choose_workspace(alias))

    def test_normal_tmp_alias_is_canonicalized(self):
        with tempfile.TemporaryDirectory(dir="/tmp") as name:
            binding = misc_binding.choose_workspace(name)
            self.assertEqual(binding["workspace_root"], str(Path(name).resolve()))
            self.assertEqual(binding["workspace_origin"], str(Path(name)))
            self.assertEqual(misc_binding.validate_binding(binding), binding)

    def test_aggregate_exclusion_and_ancestor_limits(self):
        with self.assertRaises(ValueError):
            misc_binding.choose_workspace(self.root, [str(self.base)] * 65)
        with patch("misc_binding.MAX_ANCESTORS", 1):
            with self.assertRaisesRegex(ValueError, "ancestor limit"):
                misc_binding.choose_workspace(self.root)


if __name__ == "__main__":
    unittest.main()
