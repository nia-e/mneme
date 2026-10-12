"""Portable contracts for the finite smoke (not native Windows evidence)."""
import os
from pathlib import Path
import sys
import tempfile
import time
from types import SimpleNamespace
import unittest
from unittest.mock import patch

import windows_smoke as smoke


class FiniteSmokeTests(unittest.TestCase):
    def setUp(self):
        self.temp = tempfile.TemporaryDirectory()
        self.addCleanup(self.temp.cleanup)
        self.root = Path(self.temp.name).resolve()
        self.records = []

    def child(self, source, *, seconds=5):
        child = smoke.Child([sys.executable, "-u", "-c", source], cwd=self.root,
            env=os.environ.copy(), deadline=time.monotonic() + seconds,
            records=self.records, label="fixture")
        self.addCleanup(child.close)
        return child

    def test_separate_binary_streams_and_exit_are_preserved(self):
        child = self.child("import sys; data=sys.stdin.buffer.read(); sys.stdout.buffer.write(data); sys.stderr.buffer.write(b'err\\xff')")
        child.write(b"hello\x00\xff")
        out, err = child.finish()
        self.assertEqual((out, err, child.process.returncode), (b"hello\x00\xff", b"err\xff", 0))
        child.close()
        self.assertEqual(self.records[0]["stdout_sha256"], smoke.sha(out))
        self.assertEqual(self.records[0]["stderr_sha256"], smoke.sha(err))
        child.close()
        self.assertEqual(len(self.records), 1)

    def test_response_framing_does_not_drop_buffered_second_line(self):
        child = self.child("import sys; sys.stdout.buffer.write(b'first\\nsecond\\n'); sys.stdout.flush()")
        self.assertEqual(child.line(), b"first")
        self.assertEqual(child.line(), b"second")
        child.finish()

    def test_stderr_flood_cannot_deadlock_stdout(self):
        child = self.child("import sys; sys.stderr.buffer.write(b'e'*131072); sys.stderr.flush(); print('done')")
        self.assertEqual(child.line(), b"done")
        _, err = child.finish()
        self.assertEqual(len(err), 131072)

    def test_output_bound_and_deadline_both_terminate_owned_child(self):
        child = self.child("import sys,time; sys.stderr.buffer.write(b'x'*65536); sys.stderr.flush(); time.sleep(30)")
        with patch.object(smoke, "MAX_OUTPUT", 1024):
            with self.assertRaisesRegex(RuntimeError, "output exceeds bound"):
                child.finish()
        child.close()
        self.assertIsNotNone(child.process.poll())
        quiet = self.child("import time; time.sleep(30)", seconds=0.15)
        with self.assertRaisesRegex(RuntimeError, "deadline"):
            quiet.finish()
        quiet.close()
        self.assertIsNotNone(quiet.process.poll())

    def test_tree_cleanup_after_leader_exit_removes_ordinary_descendant(self):
        marker = self.root / "orphan-marker"
        descendant = "import time; from pathlib import Path; time.sleep(0.6); Path(%r).write_text('orphan')" % str(marker)
        child = self.child("import subprocess,sys; subprocess.Popen([sys.executable,'-c',%r]); print('ready', flush=True)" % descendant)
        self.assertEqual(child.line(), b"ready")
        child.process.wait(timeout=3)
        child.close()
        time.sleep(0.8)
        self.assertFalse(marker.exists())

    def test_environment_drops_provider_keys_and_remaps_all_user_roots(self):
        with patch.dict(os.environ, {"OPENAI_API_KEY": "secret", "SOME_TOKEN": "secret", "MNEME_DB": "live",
                                    "CODEX_HOME": "live", "HF_HOME": "live", "XDG_DATA_HOME": "live"}):
            env = smoke.isolated_environment(self.root, Path(sys.executable))
        for key in ("OPENAI_API_KEY", "SOME_TOKEN", "MNEME_DB"):
            self.assertNotIn(key, env)
        for key in ("HOME", "USERPROFILE", "APPDATA", "LOCALAPPDATA", "CODEX_HOME", "HF_HOME", "TMP", "XDG_DATA_HOME"):
            self.assertTrue(Path(env[key]).is_relative_to(self.root), key)
        self.assertEqual(env["HF_ENDPOINT"], "http://127.0.0.1:9")

    def test_existing_receipt_is_never_overwritten(self):
        receipt = self.root / "receipt.json"
        original = b'{"status":"historical evidence"}\n'
        receipt.write_bytes(original)
        with self.assertRaisesRegex(RuntimeError, "receipt exists"):
            smoke.main(["--cli", "missing", "--mcp", "missing", "--output", str(receipt), "--build-description", "test"])
        self.assertEqual(receipt.read_bytes(), original)

    def test_installed_artifact_identity_checks_helpers_and_binaries(self):
        def entry(path, data):
            return {"path": str(self.root / path), "bytes": len(data), "sha256": smoke.sha(data)}
        sources = [entry("source/core_hook.py", b"current helper")]
        binaries = {"cli": entry("supplied-cli", b"current cli"), "mcp": entry("supplied-mcp", b"current mcp")}
        suffix = ".exe" if os.name == "nt" else ""
        installed = [entry("prefix/lib/core_hook.py", b"current helper"),
                     entry("prefix/bin/mnemed" + suffix, b"current cli"),
                     entry("prefix/bin/mneme-mcp" + suffix, b"current mcp")]
        smoke.verify_installed_artifacts(installed, sources, binaries)
        for stale in (entry("prefix/lib/core_hook.py", b"old embedded helper"),
                      entry("prefix/bin/mnemed" + suffix, b"different cli"),
                      entry("prefix/lib/unknown.py", b"unexpected helper")):
            with self.subTest(path=stale["path"]), self.assertRaisesRegex(RuntimeError, "differs from supplied"):
                smoke.verify_installed_artifacts([stale], sources, binaries)

    def test_installed_command_receives_explicit_input_and_environment(self):
        receipt = {"processes": []}
        fixture = smoke.Smoke(SimpleNamespace(cli=Path(sys.executable), mcp=Path(sys.executable), timeout=30), self.root, receipt)
        self.addCleanup(fixture.close)
        program = "import os,sys,json; print(json.dumps({'input':sys.stdin.read(),'pin':os.environ['FIXTURE_PIN']}))"
        result = fixture.command("fixture.installed", [sys.executable, "-c", program],
            input_data=b"explicit event", env={**fixture.env, "FIXTURE_PIN": "installed"})
        self.assertEqual(result, {"input": "explicit event", "pin": "installed"})
        self.assertNotIn("FIXTURE_PIN", fixture.env)

    def test_one_owner_cleanup_error_does_not_skip_the_other_owner(self):
        fixture = smoke.Smoke.__new__(smoke.Smoke)
        fixture.clients, fixture.children, fixture.receipt = [], [], {}
        fixture.services = {"project": {"stopped": False}, "reference": {"stopped": False}}
        attempted = []
        def stop(name):
            attempted.append(name)
            if name == "project":
                raise RuntimeError("synthetic identity refusal")
            fixture.services[name]["stopped"] = True
        with patch.object(fixture, "stop_service", side_effect=stop):
            with self.assertRaisesRegex(RuntimeError, "cleanup incomplete"):
                fixture.close()
        self.assertEqual(attempted, ["project", "reference"])
        self.assertTrue(fixture.receipt["cleanup"]["named_extra_owner_stopped"])
        self.assertIn("identity refusal", fixture.receipt["cleanup"]["errors"][0])

    @unittest.skipIf(os.name == "nt", "Unix must not impersonate native Windows")
    def test_require_windows_refuses_unix_before_artifact_execution(self):
        with self.assertRaisesRegex(RuntimeError, "native Windows required"):
            smoke.main(["--cli", "missing", "--mcp", "missing", "--output", str(self.root / "new.json"),
                        "--build-description", "test", "--require-windows"])


if __name__ == "__main__":
    unittest.main()
