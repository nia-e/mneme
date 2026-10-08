"""Provider-free guardrails for the finite host probe CLI."""

from __future__ import annotations

import contextlib
import importlib.util
import io
from pathlib import Path
import shlex
import tempfile
import unittest
from unittest.mock import patch

PROBE = Path(__file__).with_name("async_reader_session_probe.py")
spec = importlib.util.spec_from_file_location("async_reader_session_probe", PROBE)
probe = importlib.util.module_from_spec(spec)
assert spec.loader is not None
spec.loader.exec_module(probe)


class ProbeCliTests(unittest.TestCase):
    def test_hook_command_preserves_space_and_shell_metacharacters(self):
        root = Path("/tmp/a user's fixture; not-a-command")
        python = Path("/tmp/python environment/bin/python")
        with patch.object(probe, "PYTHON", python):
            argv = shlex.split(probe.hook_command(root, "PostToolUse"))
        self.assertEqual(argv, [str(python), str(PROBE.resolve()), "--hook",
                               str(root), "PostToolUse"])

    def test_help_is_inert(self):
        with patch.object(probe, "fixture_main") as fixture, patch.object(probe, "bundle_main") as bundle:
            with contextlib.redirect_stdout(io.StringIO()), self.assertRaises(SystemExit) as exit:
                probe.main(["--help"])
            self.assertEqual(exit.exception.code, 0)
            fixture.assert_not_called()
            bundle.assert_not_called()

    def test_unknown_and_missing_mode_are_inert(self):
        with patch.object(probe, "fixture_main") as fixture, patch.object(probe, "bundle_main") as bundle:
            for argv in (["--bogus"], []):
                with contextlib.redirect_stderr(io.StringIO()), self.assertRaises(SystemExit) as exit:
                    probe.main(argv)
                self.assertEqual(exit.exception.code, 2)
            fixture.assert_not_called()
            bundle.assert_not_called()

    def test_modes_are_explicit(self):
        with tempfile.TemporaryDirectory() as temp:
            receipt = Path(temp) / "fresh.json"
            with patch.object(probe, "fixture_main") as fixture, patch.object(probe, "bundle_main") as bundle:
                probe.main(["--fixture", "--output", str(receipt)])
                fixture.assert_called_once_with(receipt)
                bundle.assert_not_called()
                probe.main(["--bundle-preflight-current", "--output", str(receipt)])
                bundle.assert_called_once_with(preflight=True, current=True, output=receipt)

    def test_existing_actor_receipts_refused_before_spawn(self):
        with tempfile.TemporaryDirectory() as temp:
            receipt = Path(temp) / "existing.json"
            receipt.write_text("retained\n")
            with patch.object(probe.subprocess, "Popen", side_effect=AssertionError("actor spawned")):
                with self.assertRaisesRegex(RuntimeError, "refusing to overwrite"):
                    probe.fixture_main(receipt)
                with self.assertRaisesRegex(RuntimeError, "refusing to overwrite"):
                    probe.bundle_main(output=receipt)
            self.assertEqual(receipt.read_text(), "retained\n")


if __name__ == "__main__":
    unittest.main()
