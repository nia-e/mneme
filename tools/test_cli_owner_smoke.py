"""Harness boundary tests; these do not impersonate a native owner smoke pass."""
import argparse
import json
import os
from pathlib import Path
import tempfile
import time
import unittest
from unittest.mock import patch

import cli_owner_smoke as smoke


class Harness(unittest.TestCase):
    def setUp(self):
        self.temp = tempfile.TemporaryDirectory(prefix="mneme-owner-harness-test-")
        self.root = Path(self.temp.name)
        self.binary = self.root / "fake-cli"
        self.receipt = {"commands": [], "tools": [], "hosts": [], "checks": []}
        self.environment = patch.dict(os.environ, dict(os.environ), clear=True)
        self.environment.start()
        self.fixture = smoke.Smoke(argparse.Namespace(cli=self.binary, mcp=self.binary, timeout=30), self.root, self.receipt)

    def tearDown(self):
        self.fixture.close()
        self.environment.stop()
        self.temp.cleanup()

    def program(self, source):
        self.binary.write_text("#!/usr/bin/env python3\n" + source)
        self.binary.chmod(0o700)

    def test_success_receipt_hashes_output_without_body(self):
        body = "this fixture body should never be copied into a receipt"
        self.program("import json\nprint(json.dumps({'body': " + repr(body) + "}))\n")
        value = self.fixture.command("fixture.read", ["get", "fixture-id"])
        self.assertEqual(value["body"], body)
        self.assertNotIn(body, json.dumps(self.receipt))
        self.assertEqual(self.receipt["commands"][0]["exit"], 0)
        self.assertTrue(self.fixture.children[0].poll() is not None)

    def test_refusal_records_expectation_without_raw_diagnostic(self):
        self.program("import sys\nprint('synthetic refusal secret', file=sys.stderr)\nsys.exit(2)\n")
        self.fixture.command("fixture.refusal", ["get"], ok=False)
        self.assertNotIn("synthetic refusal secret", json.dumps(self.receipt))
        self.assertFalse(self.receipt["commands"][0]["expected_success"])
        self.assertEqual(self.receipt["commands"][0]["exit"], 2)

    def test_timeout_kills_child(self):
        self.program("import time\ntime.sleep(30)\n")
        self.fixture.deadline = time.monotonic() + 0.1
        with self.assertRaisesRegex(RuntimeError, "timeout"):
            self.fixture.command("fixture.timeout", [])
        self.assertIsNotNone(self.fixture.children[0].poll())

    def test_expired_deadline_does_not_spawn(self):
        self.program("print('{}')\n")
        self.fixture.deadline = time.monotonic() - 1
        with self.assertRaisesRegex(RuntimeError, "deadline"):
            self.fixture.command("fixture.expired", [])
        self.assertEqual(self.fixture.children, [])

    def test_output_bound_kills_child(self):
        self.program("import os, time\nos.write(1, b'x' * 8192)\ntime.sleep(30)\n")
        with patch.object(smoke, "MAX_OUTPUT", 1024):
            with self.assertRaisesRegex(RuntimeError, "exceeded bound"):
                self.fixture.command("fixture.output-bound", [])
        self.assertIsNotNone(self.fixture.children[0].poll())

    def cache(self):
        source = self.root / "model-inputs"
        model = source / "models--Xenova--bge-base-en-v1.5"
        (model / "refs").mkdir(parents=True)
        (model / "refs/main").write_bytes(b"a" * 40)
        snapshot = model / "snapshots" / ("a" * 40)
        for name in ("onnx/model.onnx", "tokenizer.json", "config.json", "special_tokens_map.json", "tokenizer_config.json"):
            path = snapshot / name
            path.parent.mkdir(parents=True, exist_ok=True)
            path.write_bytes(b"synthetic model input")
        self.fixture.options.embedding_cache = source
        return source, snapshot

    def test_cache_inputs_are_copied_not_borrowed(self):
        source, snapshot = self.cache()
        before = smoke.tree_hash(source)
        self.fixture.prepare_cache()
        self.assertEqual(len(self.receipt["model_inputs"]["assets"]), 5)
        self.assertEqual(smoke.tree_hash(source), before)
        copied = Path(self.receipt["model_inputs"]["disposable_cache"])
        self.assertTrue((copied / snapshot.relative_to(source) / "onnx/model.onnx").is_file())
        self.assertEqual(self.fixture.env["HF_ENDPOINT"], "http://127.0.0.1:9")

    def test_cache_refusal_precedes_copy(self):
        source, snapshot = self.cache()
        (snapshot / "onnx/model.onnx").unlink()
        with self.assertRaisesRegex(RuntimeError, "missing"):
            self.fixture.prepare_cache()
        self.assertFalse((self.fixture.root / "cache/hf").exists())

    def test_transport_failure_is_not_a_tool_refusal(self):
        class Client:
            def call_tool(self, name, arguments):
                raise smoke.McpTransportError("synthetic owner disappeared")
        with self.assertRaisesRegex(RuntimeError, "transport failure"):
            self.fixture.tool(Client(), "get", {}, ok=False)

    def test_cli_signal_is_not_a_refusal(self):
        self.program("import os, signal\nos.kill(os.getpid(), signal.SIGTERM)\n")
        with self.assertRaisesRegex(RuntimeError, "crash or signal"):
            self.fixture.command("fixture.signal", [], ok=False)

    def test_helper_timeout_kills_its_private_descendants(self):
        marker = self.root / "orphan-marker"
        grandchild = "import time; from pathlib import Path; time.sleep(0.5); Path(" + repr(str(marker)) + ").write_text('orphan')"
        self.program("import subprocess, sys, time\nsubprocess.Popen([sys.executable, '-c', " + repr(grandchild) + "])\ntime.sleep(30)\n")
        self.fixture.deadline = time.monotonic() + 0.1
        with self.assertRaisesRegex(RuntimeError, "timeout"):
            self.fixture.command("fixture.helper-timeout", [str(self.binary)], executable=Path(smoke.sys.executable))
        time.sleep(0.65)
        self.assertFalse(marker.exists(), "helper timeout left its native-style grandchild alive")

    def test_tree_hash_tracks_empty_directories(self):
        before = smoke.tree_hash(self.fixture.blank)
        (self.fixture.blank / "empty").mkdir()
        self.assertNotEqual(before, smoke.tree_hash(self.fixture.blank))


if __name__ == "__main__":
    unittest.main()
