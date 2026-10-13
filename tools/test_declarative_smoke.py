"""Pure process/fixture safety checks; not another native smoke suite."""
import importlib.util
from pathlib import Path
import socket
import sys
import tempfile
import time
from types import SimpleNamespace
import unittest
from unittest.mock import patch

spec = importlib.util.spec_from_file_location("declarative_smoke", Path(__file__).with_name("declarative_smoke.py"))
smoke_module = importlib.util.module_from_spec(spec)
spec.loader.exec_module(smoke_module)


class SafetyTests(unittest.TestCase):
    def test_environment_is_disposable_and_credentials_removed(self):
        with tempfile.TemporaryDirectory() as directory, patch.dict(smoke_module.os.environ,
                {"OPENAI_API_KEY": "not-a-real-key", "MNEME_DB": "/must/not/open", "EXAMPLE_SECRET": "no"}):
            root = Path(directory)
            env = smoke_module.isolated_env(root, root / "mnemed")
            self.assertNotIn("OPENAI_API_KEY", env)
            self.assertNotIn("EXAMPLE_SECRET", env)
            self.assertNotIn("MNEME_DB", env)
            self.assertTrue(Path(env["HOME"]).is_relative_to(root))
            self.assertEqual(env["HF_HUB_OFFLINE"], "1")

    def test_contact_trap_counts_connections(self):
        trap = smoke_module.Trap()
        try:
            with socket.create_connection(trap.socket.getsockname(), timeout=1):
                pass
            deadline = time.monotonic() + 1
            while trap.count == 0 and time.monotonic() < deadline:
                time.sleep(.01)
            self.assertEqual(trap.count, 1)
        finally:
            trap.close()
        self.assertFalse(trap.thread.is_alive())

    def test_oversized_child_output_is_bounded_and_child_cleaned(self):
        with tempfile.TemporaryDirectory() as directory:
            receipt = {}
            options = SimpleNamespace(cli=Path(sys.executable), mcp=Path(sys.executable), timeout=30)
            smoke = smoke_module.Smoke(options, Path(directory), receipt)
            receipt["commands"] = []
            try:
                with patch.object(smoke_module, "MAX_OUTPUT", 64):
                    with self.assertRaisesRegex(RuntimeError, "output too large"):
                        smoke.run_command("oversize", [sys.executable, "-B", "-c", "print('x'*100)"])
                self.assertTrue(all(child.poll() is not None for child in smoke.children))
            finally:
                smoke.close()


if __name__ == "__main__":
    unittest.main()
