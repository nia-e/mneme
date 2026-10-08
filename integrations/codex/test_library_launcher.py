import json
import os
from pathlib import Path
import tempfile
import unittest
from unittest.mock import patch

import library_launcher


class LibraryLauncherTests(unittest.TestCase):
    def setUp(self):
        self.temp = tempfile.TemporaryDirectory()
        self.addCleanup(self.temp.cleanup)
        self.root = Path(self.temp.name).resolve()
        self.project = self.root / "project"
        self.cwd = self.project / "src"
        self.cwd.mkdir(parents=True)
        (self.project / ".mneme").mkdir()
        self.personal = self.root / "personal-library.json"
        self.personal.write_text("{}")
        self.independent = self.root / "independent-library.json"
        self.independent.write_text("{}")
        self.binary = self.root / "mneme-mcp"
        self.binary.write_text("#!/bin/sh\n")
        self.binary.chmod(0o700)
        self.args = ["--binary", str(self.binary),
                     "--default-library-config", str(self.personal)]

    def profile(self, mode, **extra):
        (self.project / ".mneme/profile.json").write_text(json.dumps({
            "schema": "mneme.profile.v1", "mode": mode, **extra}))

    def run_wrapper(self):
        with patch.object(os, "getcwd", return_value=str(self.cwd)):
            return library_launcher.main(self.args)

    def test_absent_profile_retains_default_personal_library(self):
        with patch.object(library_launcher.os, "execv") as execv:
            self.assertIsNone(self.run_wrapper())
        execv.assert_called_once_with(str(self.binary),
                                      [str(self.binary), "--library-config", str(self.personal)])

    def test_private_without_selection_keeps_default_but_explicit_selection_replaces_it(self):
        self.profile("private")
        self.assertEqual(library_launcher.select_library(self.cwd, str(self.personal)), self.personal)
        self.profile("private", library_config=str(self.independent))
        self.assertEqual(library_launcher.select_library(self.cwd, str(self.personal)), self.independent)

    def test_isolated_without_library_refuses_before_native_spawn(self):
        self.profile("isolated")
        with patch.object(library_launcher.os, "execv") as execv:
            self.assertEqual(self.run_wrapper(), 1)
        execv.assert_not_called()

    def test_isolated_cannot_alias_personal_default(self):
        alias = self.root / "alias.json"
        alias.symlink_to(self.personal)
        self.profile("isolated", library_config=str(alias))
        with patch.object(library_launcher.os, "execv") as execv:
            self.assertEqual(self.run_wrapper(), 1)
        execv.assert_not_called()
        alias.unlink()
        os.link(self.personal, alias)
        with patch.object(library_launcher.os, "execv") as execv:
            self.assertEqual(self.run_wrapper(), 1)
        execv.assert_not_called()

    def test_isolated_explicit_independent_library(self):
        self.profile("isolated", library_config=str(self.independent))
        with patch.object(library_launcher.os, "execv") as execv:
            self.assertIsNone(self.run_wrapper())
        execv.assert_called_once_with(str(self.binary),
                                      [str(self.binary), "--library-config", str(self.independent)])

    def test_invalid_profile_never_spawns_default(self):
        (self.project / ".mneme/profile.json").write_text('{"schema":"bad"}')
        with patch.object(library_launcher.os, "execv") as execv:
            self.assertEqual(self.run_wrapper(), 1)
        execv.assert_not_called()


if __name__ == "__main__":
    unittest.main()
