"""Executable selection fixtures only; no real owner or MCP provider."""
import os
from pathlib import Path
import tempfile
import unittest
from unittest import mock

import mcp_client
from host_platform import executable_name


class ClientBinaryTests(unittest.TestCase):
    def setUp(self):
        temporary = tempfile.TemporaryDirectory()
        self.addCleanup(temporary.cleanup)
        self.base = Path(temporary.name).resolve()
        helper = self.base / "bundle" / "python" / "mcp_client.py"
        helper.parent.mkdir(parents=True)
        self.sibling = helper.parent.parent / "bin" / executable_name("mnemed")
        self.sibling.parent.mkdir()
        self.sibling.write_bytes(b"packaged sibling")
        self.addCleanup(mock.patch.stopall)
        mock.patch.object(mcp_client, "__file__", str(helper)).start()
        mock.patch.dict(os.environ, {}, clear=False).start()
        os.environ.pop("MNEME_CLIENT_BINARY", None)

    def test_path_precedes_packaged_sibling_and_is_resolved_for_each_new_child(self):
        with mock.patch.object(mcp_client, "find_executable", side_effect=["/current/mnemed", "/new/mnemed"]):
            self.assertEqual(mcp_client._binary(), "/current/mnemed")
            self.assertEqual(mcp_client._binary(), "/new/mnemed")

    def test_override_wins_without_path_discovery(self):
        override = "/explicit/" + executable_name("current")
        with mock.patch.dict(os.environ, {"MNEME_CLIENT_BINARY": override}), \
                mock.patch.object(mcp_client, "find_executable") as find:
            self.assertEqual(mcp_client._binary(), override)
            find.assert_not_called()

    def test_missing_path_uses_sibling_and_missing_both_reports_transport_error(self):
        with mock.patch.object(mcp_client, "find_executable", return_value=None):
            self.assertEqual(mcp_client._binary(), str(self.sibling))
            self.sibling.unlink()
            with self.assertRaisesRegex(mcp_client.McpTransportError, "binary unavailable"):
                mcp_client._binary()

    def test_failed_selected_path_does_not_retry_sibling(self):
        with mock.patch.object(mcp_client, "find_executable", return_value="/current/broken"), \
                mock.patch.object(mcp_client.host_process, "spawn", side_effect=OSError("broken")) as spawn:
            client = mcp_client.McpClient("http://127.0.0.1:1234")
            with self.assertRaisesRegex(mcp_client.McpTransportError, "process unavailable"):
                client._start()
            self.assertEqual(spawn.call_count, 1)
            self.assertEqual(spawn.call_args.args[0][0], "/current/broken")

    @unittest.skipUnless(os.name == "posix", "POSIX executable fixture")
    def test_actual_path_order_and_updated_symlink_are_not_pinned(self):
        first, second = self.base / "first", self.base / "second"
        first.mkdir()
        second.mkdir()
        target = first / "v1"
        target.write_text("#!/bin/sh\nexit 0\n")
        target.chmod(0o700)
        link = first / "mnemed"
        link.symlink_to(target)
        other = second / "mnemed"
        other.write_bytes(target.read_bytes())
        other.chmod(0o700)
        with mock.patch.dict(os.environ, {"PATH": str(first)}):
            self.assertEqual(mcp_client._binary(), str(link))
            link.unlink()
            link.symlink_to(other)
            self.assertEqual(mcp_client._binary(), str(link))
            os.environ["PATH"] = str(second)
            self.assertEqual(mcp_client._binary(), str(other))


if __name__ == "__main__":
    unittest.main()
