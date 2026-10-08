"""Provider-free checks for the extracted bounded selection transport."""
import unittest
from unittest.mock import Mock, patch

from tools import selection_http as transport


class SelectionHttpTests(unittest.TestCase):
    def call(self, raw=b'{"answers":{}}', *, status=200, error=None):
        reply = Mock(status=status)
        reply.read.return_value = raw
        conn = Mock()
        conn.getresponse.return_value = reply
        if error is not None:
            conn.request.side_effect = error
        with patch.object(transport.http.client, "HTTPSConnection", return_value=conn) as create, \
             patch.object(transport.signal, "getitimer", return_value=(0, 0)), \
             patch.object(transport.signal, "getsignal", return_value="old-handler"), \
             patch.object(transport.signal, "signal") as handler, \
             patch.object(transport.signal, "setitimer") as timer:
            result = transport.call(None, b'{"fixture":true}', "synthetic-key")
        create.assert_called_once_with(transport.HOST, timeout=transport.DEADLINE_SECONDS)
        self.assertEqual(conn.request.call_count, 1)
        self.assertEqual(timer.call_args.args, (transport.signal.ITIMER_REAL, 0))
        self.assertEqual(handler.call_args.args, (transport.signal.SIGALRM, "old-handler"))
        return result, conn, reply

    def test_success_is_bounded_and_returns_reusable_connection(self):
        result, conn, reply = self.call()
        self.assertEqual(result[:3], (200, {"answers": {}}, None))
        self.assertIs(result[-1], conn)
        reply.read.assert_called_once_with(transport.MAX_RESPONSE_BYTES + 1)
        conn.close.assert_not_called()

    def test_http_failure_discards_connection_without_retry(self):
        result, conn, _ = self.call(status=429)
        self.assertEqual(result[:3], (429, None, "HTTP 429"))
        self.assertIsNone(result[-1])
        conn.close.assert_called_once()

    def test_failed_request_discards_connection_without_exception_text(self):
        result, conn, _ = self.call(error=RuntimeError("private diagnostic"))
        self.assertEqual(result[:3], (None, None, "RuntimeError"))
        self.assertNotIn("private diagnostic", repr(result))
        conn.close.assert_called_once()

    def test_credential_echo_and_oversize_are_not_retained(self):
        for raw in (b'{"echo":"synthetic-key"}', b"x" * (transport.MAX_RESPONSE_BYTES + 1)):
            with self.subTest(length=len(raw)):
                result, conn, _ = self.call(raw)
                self.assertEqual(result[:3], (200, None, "ValueError"))
                self.assertIsNone(result[-1])
                conn.close.assert_called_once()


if __name__ == "__main__":
    unittest.main()
