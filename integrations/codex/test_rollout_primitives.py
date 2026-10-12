"""Portable bounded reads without transcript discovery or state mutation."""
import os
from pathlib import Path
import tempfile
import unittest

import rollout_primitives as rollout


class RolloutFileTests(unittest.TestCase):
    def setUp(self):
        self.temp = tempfile.TemporaryDirectory()
        self.addCleanup(self.temp.cleanup)
        self.root = Path(self.temp.name)

    def test_binary_read_preserves_crlf_and_sentinel_byte(self):
        path = self.root / "events.jsonl"
        path.write_bytes(b"a\r\nb\r\n")
        self.assertEqual(rollout.read_regular(path, 3), b"a\r\nb")
        self.assertEqual(rollout.read_regular(path, 20), b"a\r\nb\r\n")
        self.assertEqual(path.read_bytes(), b"a\r\nb\r\n")

    def test_absent_or_nonregular_source_is_not_created(self):
        path = self.root / "missing"
        with self.assertRaises(FileNotFoundError):
            rollout.open_regular(path)
        self.assertFalse(path.exists())
        with self.assertRaises((ValueError, OSError)):
            rollout.open_regular(self.root)

    def test_readonly_hardlinked_source_preserves_existing_admission(self):
        path, alias = self.root / "source", self.root / "alias"
        path.write_bytes(b"original")
        os.link(path, alias)
        self.assertEqual(rollout.read_regular(alias, 50), b"original")
        self.assertEqual(path.read_bytes(), b"original")

    def test_symlink_source_is_rejected(self):
        path, alias = self.root / "source", self.root / "alias"
        path.write_bytes(b"original")
        try:
            alias.symlink_to(path)
        except OSError as error:
            if os.name == "nt" and error.winerror == 1314:
                self.skipTest("Windows symlink privilege unavailable")
            raise
        with self.assertRaises((ValueError, OSError)):
            rollout.read_regular(alias, 50)

    @unittest.skipIf(os.name == "nt", "Unix FIFO boundary")
    def test_fifo_source_is_rejected_without_waiting_for_writer(self):
        path = self.root / "fifo"
        os.mkfifo(path)
        with self.assertRaises(ValueError):
            rollout.read_regular(path, 50)


if __name__ == "__main__":
    unittest.main()
