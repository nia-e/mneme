"""Real native discovery across setup boundaries; no inference or running owner.

Build the disposable-store CLI with:
  cargo build -p mnemed --no-default-features --features cozo
Set MNEME_TEST_NATIVE_BINARY to test another explicitly selected artifact.
The HTTP publisher mirrors mneme-store-client/src/discovery/fixture.rs; native
selection, receipt acquisition/replay, store creation and installation are real.
"""
import argparse
from collections import Counter
import hashlib
from http.server import BaseHTTPRequestHandler, HTTPServer
import json
import os
from pathlib import Path
import subprocess
import sys
import tempfile
import threading
import unittest
from unittest.mock import patch

if sys.version_info < (3, 11):
    raise unittest.SkipTest("Native setup discovery tests require Python 3.11+ for TOML parsing")

from host_files import lock_file, unlock_file
import launcher
import project_setup
from service import load_config
from store_registry import NativeSelection, native_bridge, native_select


PUBLISHER_ID = "01ARZ3NDEKTSV4RRFFQ69G5FA1"


class DiscoveryPublisher:
    """One finite loopback metadata publisher; no database or model behind it."""

    def __init__(self, root):
        self.root = root
        self.requests = []
        self.acquisitions = []
        self.errors = []
        self.document = {
            "schema": "mneme.discovery-catalog.v1",
            "publisher_db_id": PUBLISHER_ID,
            "catalog_sha256": hashlib.sha256(b"{}").hexdigest(),
            "stores": {},
        }
        publisher = self

        class Handler(BaseHTTPRequestHandler):
            protocol_version = "HTTP/1.1"

            def log_message(self, *_args):
                pass

            def do_POST(self):
                self.connection.settimeout(2)
                try:
                    size = int(self.headers.get("Content-Length", "0"))
                    if not 0 < size <= 128 * 1024 or len(publisher.requests) >= 64:
                        raise ValueError("publisher request bound exceeded")
                    message = json.loads(self.rfile.read(size))
                    publisher.requests.append(message)
                    result = publisher.respond(message)
                    if "id" not in message:
                        self.send_response(202)
                        raw = b""
                    else:
                        self.send_response(200)
                        raw = json.dumps({"jsonrpc": "2.0", "id": message["id"],
                                          "result": result}).encode()
                    self.send_header("Content-Type", "application/json")
                    self.send_header("Mcp-Session-Id", "native-setup-discovery-fixture")
                    self.send_header("Content-Length", str(len(raw)))
                    self.send_header("Connection", "close")
                    self.end_headers()
                    self.wfile.write(raw)
                except Exception as error:
                    publisher.errors.append(repr(error))
                    self.send_error(400)
                self.close_connection = True

            def do_DELETE(self):
                self.send_response(204)
                self.send_header("Content-Length", "0")
                self.send_header("Connection", "close")
                self.end_headers()
                self.close_connection = True

        self.server = HTTPServer(("127.0.0.1", 0), Handler)
        self.server.timeout = .1
        self.url = f"http://127.0.0.1:{self.server.server_port}/mcp"
        self.thread = threading.Thread(target=self.server.serve_forever,
                                       kwargs={"poll_interval": .01}, daemon=True)
        self.thread.start()

    def close(self):
        self.server.shutdown()
        self.server.server_close()
        self.thread.join(timeout=3)
        if self.thread.is_alive():
            raise RuntimeError("discovery publisher did not stop")

    def respond(self, message):
        method = message.get("method")
        if method == "initialize":
            return {"protocolVersion": "2025-11-25", "capabilities": {"tools": {}},
                    "serverInfo": {"name": "mneme-mcp", "version": "fixture"},
                    "capabilityProfile": "operator"}
        if method == "notifications/initialized":
            return None
        if method == "tools/list":
            return {"tools": [
                {"name": "databases", "inputSchema": {
                    "type": "object", "properties": {}, "additionalProperties": False}},
                {"name": "discovery_catalog", "inputSchema": {
                    "type": "object", "additionalProperties": False,
                    "required": ["db", "expected_db_id"], "properties": {
                        "db": {"type": "string", "minLength": 1, "maxLength": 256},
                        "expected_db_id": {"type": "string", "minLength": 26,
                            "maxLength": 26, "pattern": "^[0-7][0-9A-HJKMNP-TV-Z]{25}$"},
                        "expected_catalog_sha256": {"type": "string", "minLength": 64,
                            "maxLength": 64, "pattern": "^[0-9a-f]{64}$"}}}},
            ]}
        if method != "tools/call":
            raise ValueError(f"unexpected metadata method: {method}")
        params = message["params"]
        if params["name"] == "databases":
            value = [{"db": "project", "db_id": PUBLISHER_ID, "state": "open",
                      "resolved_path": "/fixture/publisher.db"}]
        elif params["name"] == "discovery_catalog":
            if params["arguments"] != {"db": "project", "expected_db_id": PUBLISHER_ID}:
                raise ValueError("discovery request lacks exact publisher identity")
            self.acquisitions.append(((self.root / ".mneme/memory.toml").exists(),
                                      (self.root / ".codex/config.toml").exists()))
            value = self.document
        else:
            raise ValueError(f"unexpected metadata tool: {params['name']}")
        return {"content": [{"type": "text", "text": json.dumps(value)}]}


@unittest.skipUnless(os.name == "posix", "POSIX executable and actual flock lease fixture")
class NativeProjectDiscoveryTests(unittest.TestCase):
    def setUp(self):
        default = Path(__file__).resolve().parents[2] / "target/debug/mnemed"
        self.native = Path(os.environ.get("MNEME_TEST_NATIVE_BINARY", default)).resolve()
        if not self.native.is_file() or not os.access(self.native, os.X_OK):
            self.skipTest("native artifact unavailable; build cozo/no-fastembed mnemed or set MNEME_TEST_NATIVE_BINARY")
        temporary = tempfile.TemporaryDirectory(prefix="mneme-native-project-discovery-")
        self.addCleanup(temporary.cleanup)
        self.base = Path(temporary.name).resolve()
        self.root = self.base / "project-discovery"
        self.root.mkdir()
        self.bin = self.base / "bin"
        self.bin.mkdir()
        self.mcp = self.bin / "mneme-mcp"
        self.mcp.write_text("#!/bin/sh\n: > \"${MNEME_TEST_MCP_MARKER:?}\"\nexit 99\n")
        self.mcp.chmod(0o700)
        environment = {"HOME": str(self.base / "home"),
                       "XDG_DATA_HOME": str(self.base / "data"),
                       "XDG_CONFIG_HOME": str(self.base / "config"),
                       "XDG_CACHE_HOME": str(self.base / "cache"),
                       "CODEX_HOME": str(self.base / "codex-home"),
                       "CODEX_ROOT": str(self.base / "codex-root"),
                       "MNEME_TEST_MCP_MARKER": str(self.base / "unexpected-mcp-process"),
                       "PATH": os.pathsep.join((str(self.native.parent), str(self.bin), "/usr/bin", "/bin"))}
        isolated = patch.dict(os.environ, environment, clear=True)
        isolated.start()
        self.addCleanup(isolated.stop)
        self.publisher = DiscoveryPublisher(self.root)
        self.addCleanup(self.publisher.close)
        self.memory = self.base / "memory.toml"
        self.memory.write_text(
            "schema='mneme.config.v1'\n[stores.publisher]\n"
            f"id='{PUBLISHER_ID}'\nowner={{url='{self.publisher.url}',db='project'}}\n"
            "discovery=true\nactive={workspace='any'}\n")
        self.args = argparse.Namespace(
            root=self.root, mnemed=self.native, mcp_binary=self.mcp,
            memory_config=self.memory, library_config=None, store_registry=None,
            library_helper=Path(__file__).resolve().parent.parent / "library/library.py",
            python=sys.executable, codex_binary=None, port=None,
            no_recording=False, no_trust_hooks=True, json=True)

    def setup_project(self):
        result = {"schema": project_setup.SCHEMA, "status": "incomplete", "stage": "preflight"}
        with patch.object(project_setup, "ensure_ready", return_value={"state": "fixture"}) as ready:
            project_setup.setup(self.args, result)
        ready.assert_called_once()
        self.assertEqual(result["status"], "configured")
        return result

    def test_new_overlay_and_codex_boundary_acquire_fresh_then_replay_offline(self):
        initial_memory = self.memory.read_bytes()
        result = self.setup_project()
        self.assertEqual(result["integration"], "native")
        self.assertFalse(result["published"])
        self.assertEqual(result["store"], "created")
        self.assertEqual(self.memory.read_bytes(), initial_memory)
        self.assertFalse((Path(os.environ["CODEX_HOME"]) / "config.toml").exists())
        self.assertFalse((self.root / ".mneme/cli.json").exists())
        self.assertEqual(self.publisher.errors, [])
        self.assertFalse((self.base / "unexpected-mcp-process").exists())

        # Repeated preparation may reacquire one boundary; all three distinct
        # metadata states must nevertheless reach the actual publisher.
        stages = list(dict.fromkeys(self.publisher.acquisitions))
        self.assertEqual(stages, [(False, False), (True, False), (True, True)])
        calls = Counter(message["method"] for message in self.publisher.requests)
        self.assertGreaterEqual(calls["initialize"], 3)
        self.assertGreaterEqual(calls["tools/list"], 3)
        tools = Counter(message["params"]["name"] for message in self.publisher.requests
                        if message["method"] == "tools/call")
        self.assertGreaterEqual(tools["databases"], 3)
        self.assertEqual(tools["discovery_catalog"], len(self.publisher.acquisitions))
        receipts = [json.loads(path.read_bytes()) for path in
                    (self.base / "cache/mneme/discovery").glob("*.json")]
        self.assertEqual(len(receipts), 3)
        self.assertEqual(len({receipt["boundary_sha256"] for receipt in receipts}), 3)
        for receipt in receipts:
            self.assertEqual(receipt["graph"]["seeds"], [PUBLISHER_ID])
            self.assertEqual(receipt["graph"]["unavailable"], {})
            self.assertEqual(receipt["graph"]["catalogs"][PUBLISHER_ID], self.publisher.document)

        acquisitions = len(self.publisher.acquisitions)
        replay = native_select(str(self.memory), str(self.native), str(self.root),
                               expected_selection_digest=result["selection_digest"])
        self.assertEqual(replay.digest, result["selection_digest"])
        # Installation's reviewed input predates its own Codex output. It must
        # not remain the launcher's lasting selection after that boundary write.
        installed = json.loads(Path(result["receipt"]).read_bytes())
        preinstall_digest = installed["plan"]["memory_config"]["digest"]
        self.assertNotEqual(preinstall_digest, replay.digest)
        with self.assertRaises(ValueError):
            native_select(str(self.memory), str(self.native), str(self.root),
                          expected_selection_digest=preinstall_digest)
        self.assertEqual(len(self.publisher.acquisitions), acquisitions)
        witness = replay.snapshot["discovery"]
        receipt = json.loads(Path(witness["binding"]["path"]).read_bytes())
        self.assertEqual(receipt["boundary_sha256"], witness["boundary_sha256"])
        roles = {source["role"] for source in replay.snapshot["sources"]}
        self.assertTrue({"user", "project", "legacy-routing"} <= roles, roles)

        # The real native snapshot/managed-launcher guard contract can consume
        # that final receipt offline, without a synthetic selection or service.
        snapshot = native_bridge(str(self.native), ["select", "--config", str(self.memory),
                                "--cwd", str(self.root), "--offline"], timeout=3)
        selected = NativeSelection(snapshot, str(self.memory), str(self.native), str(self.root))
        service = load_config(result["service_config"])
        launcher._native_local_guard(selected, service, self.root, result["db_id"])
        self.assertEqual(selected.digest, replay.digest)
        self.assertEqual(len(self.publisher.acquisitions), acquisitions)

        # Prove an offline inspect would contend, then hold that exact lease
        # through rerun. Adoption must reuse its metadata, not reopen the DB.
        database = self.root / ".mneme/codex-memory.db"
        before = database.read_bytes()
        overlay = (self.root / ".mneme/memory.toml").read_bytes()
        with database.with_name(database.name + ".mneme.lock").open("r+b") as lease:
            lock_file(lease, blocking=False)
            try:
                inspect = subprocess.run([str(self.native), "--json", "--db", str(database),
                                          "capture", "inspect"], cwd=self.root,
                                         stdin=subprocess.DEVNULL, capture_output=True, timeout=5)
                self.assertNotEqual(inspect.returncode, 0)
                self.assertIn(b"already owned", inspect.stderr)
                again = self.setup_project()
            finally:
                unlock_file(lease)
        self.assertEqual(again["store"], "adopted")
        self.assertEqual(again["db_id"], result["db_id"])
        self.assertEqual(again["selection_digest"], result["selection_digest"])
        self.assertEqual(database.read_bytes(), before)
        self.assertEqual((self.root / ".mneme/memory.toml").read_bytes(), overlay)
        self.assertEqual(self.publisher.errors, [])

        # A configured parent startup pin cannot erase a real nested boundary.
        # Select the actual descendant request, then apply the same
        # guard as the managed controller without starting any process.
        for boundary in ("private", "isolated", "overlay"):
            with self.subTest(boundary=boundary):
                child = self.root / ("nested-" + boundary)
                (child / ".mneme").mkdir(parents=True)
                if boundary == "overlay":
                    (child / ".mneme/memory.toml").write_text("schema='mneme.config.v1'\n")
                else:
                    (child / ".mneme/profile.json").write_text(json.dumps(
                        {"schema": "mneme.profile.v1", "mode": boundary}))
                acquisitions = len(self.publisher.acquisitions)
                child_selection = native_select(str(self.memory), str(self.native), str(child))
                self.assertEqual(child_selection.snapshot["profile"],
                                 "default" if boundary == "overlay" else boundary)
                self.assertEqual(child_selection.snapshot["project"]["root"], str(child))
                if boundary == "isolated":
                    self.assertEqual(len(self.publisher.acquisitions), acquisitions)
                with self.assertRaisesRegex(ValueError, "exact configured project"):
                    launcher._native_local_guard(child_selection, service, self.root, result["db_id"],
                                                 actual_cwd=child)
        self.assertEqual(self.publisher.errors, [])


if __name__ == "__main__":
    unittest.main()
