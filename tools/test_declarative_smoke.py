"""Pure process/fixture safety checks; not another native smoke suite."""
import ast
import hashlib
import importlib.util
import json
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

    def test_background_lifecycle_seed_uses_canonical_accounting_without_provider(self):
        import reader_worker
        # Execute the actual embedded seed, not a second implementation of it.
        source = ast.parse(smoke_module.BACKGROUND_LIFECYCLE)
        seed = next(node for node in source.body if isinstance(node, ast.FunctionDef) and node.name == "seed")
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            service = root / "service.json"
            service.write_text("{}")
            config = {"state_dir": root / "state", "service_config": service,
                      "project_root": root, "reader_model": "gpt-6.1-sol", "librarian_effort": "medium"}
            namespace = {"hashlib": hashlib, "reader_worker": reader_worker, "config": config}
            exec(compile(ast.Module(body=[seed], type_ignores=[]), "BACKGROUND_LIFECYCLE.seed", "exec"), namespace)
            session = "disposable-smoke-seed"
            self.assertTrue(reader_worker._state(config, session,
                lambda data: (data.update(ended=True), True))[0])
            with patch("assessment.runtime") as runtime:
                self.assertTrue(reader_worker._state(config, session, namespace["seed"])[0])
            runtime.assert_not_called()
            path, _ = reader_worker._paths(config, session)
            data = json.loads(path.read_bytes())
            self.assertEqual([data[key] for key in ("attempts", "input_tokens", "output_tokens")], [1, 17, 3])
            self.assertTrue(data["ended"])
            self.assertEqual(data["pending"]["turn"], "fixture")
            self.assertIsNone(data["reservation"])
            def inspect(data):
                ledger = data["_accounting"]
                packet = ledger.lookup("user_message", "fixture", hashlib.sha256(b"").hexdigest())
                return (ledger.usage(packet), ledger.db.execute(
                    "SELECT status FROM calls WHERE id='smoke:ended-fixture'").fetchone()[0]), False
            ok, (usage, status) = reader_worker._state(config, session, inspect)
            self.assertTrue(ok)
            self.assertEqual([usage[key] for key in ("attempts", "input_tokens", "output_tokens")], [1, 17, 3])
            self.assertEqual(status, "settled")
            reader_worker.reset(config, session)
            reader_worker.end_session(config, session)
            data = json.loads(path.read_bytes())
            self.assertEqual([data[key] for key in ("attempts", "input_tokens", "output_tokens")], [1, 17, 3])

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
