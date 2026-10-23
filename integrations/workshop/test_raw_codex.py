"""Only disposable package layouts and mocked exec; no actual Codex launch."""
import json
import platform
import sys
from pathlib import Path
import tempfile
import unittest
from unittest.mock import patch

import codex_guard
import raw_codex


class PackageSelectionTests(unittest.TestCase):
    def setUp(self):
        temporary = tempfile.TemporaryDirectory()
        self.addCleanup(temporary.cleanup)
        self.root = Path(temporary.name).resolve()

    def package(self, version):
        package = self.root / version
        (package / "bin").mkdir(parents=True)
        for name in ("codex-resources", "codex-path"):
            (package / name).mkdir()
        binary = package / "bin/codex"
        binary.write_text("fake executable")
        binary.chmod(0o700)
        (package / "codex-package.json").write_text(json.dumps({
            "layoutVersion": 1, "version": version, "variant": "codex",
            "target": ({"arm64": "aarch64", "amd64": "x86_64"}.get(platform.machine().lower(), platform.machine().lower())
                       + ("-apple-darwin" if sys.platform == "darwin" else "-unknown-linux-musl")), "entrypoint": "bin/codex",
            "resourcesDir": "codex-resources", "pathDir": "codex-path"}))
        (package / "release-manifest.json").write_text(json.dumps({
            "schema": "mneme.codex-install.v1", "release": {
                "tag_name": "rust-v" + version, "draft": False, "prerelease": False}}))
        return binary

    def test_numeric_precedence_and_incomplete_latest_skipped(self):
        older = self.package("0.9.0")
        newest = self.package("0.10.0")
        incomplete = self.package("1.0.0")
        incomplete.parent.parent.joinpath("release-manifest.json").unlink()
        self.assertEqual(raw_codex.select_package(self.root), newest)
        newest.unlink()
        self.assertEqual(raw_codex.select_package(self.root), older)

    def test_prerelease_is_not_a_version_approval_gate(self):
        self.package("0.1.0")
        preview = self.package("0.2.0-alpha.10")
        self.package("0.2.0-alpha.2")
        manifest = preview.parent.parent / "release-manifest.json"
        value = json.loads(manifest.read_text())
        value["release"].update(prerelease=True, draft=True)
        manifest.write_text(json.dumps(value))
        self.assertEqual(raw_codex.select_package(self.root), preview)
        final = self.package("0.2.0")
        self.assertEqual(raw_codex.select_package(self.root), final)

    def test_target_metadata_does_not_silently_select_older_edition(self):
        old = self.package("0.1.0")
        new = self.package("0.2.0")
        metadata = new.parent.parent / "codex-package.json"
        value = json.loads(metadata.read_text())
        value["target"] = "incompatible-unknown-linux-musl"
        metadata.write_text(json.dumps(value))
        self.assertEqual(raw_codex.select_package(self.root), new)

    def test_selection_refreshes_but_existing_result_stays_frozen(self):
        old = self.package("0.1.0")
        selected = codex_guard.resolve_raw_codex(None, self.root)
        new = self.package("0.2.0")
        self.assertEqual(selected, old)
        self.assertEqual(codex_guard.resolve_raw_codex(None, self.root), new)

    def test_malformed_layout_and_symlink_escape_are_skipped(self):
        old = self.package("0.1.0")
        new = self.package("0.2.0")
        new.unlink()
        new.symlink_to(old)
        self.assertEqual(raw_codex.select_package(self.root), old)
        (old.parent.parent / "codex-package.json").write_text("[]")
        with self.assertRaises(raw_codex.SelectionError):
            raw_codex.select_package(self.root)

    def test_cli_resolves_and_execs_without_guard_bypass_claim(self):
        binary = self.package("0.1.0")
        with patch.object(raw_codex.os, "execvpe", side_effect=OSError("fake exec")) as launch:
            self.assertEqual(raw_codex.main(["--packages", str(self.root), "--", "--version"]), 1)
        self.assertEqual(launch.call_args.args[:2], (str(binary), [str(binary), "--version"]))

    def test_absent_or_relative_inventory_is_an_ordinary_error(self):
        with self.assertRaises(raw_codex.SelectionError):
            raw_codex.select_package(Path("relative"))
        with self.assertRaises(FileNotFoundError):
            raw_codex.select_package(self.root / "absent")
        self.assertEqual(raw_codex.main(["--packages", str(self.root), "--"]), 1)
