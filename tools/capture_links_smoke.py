#!/usr/bin/env python3
"""Finite installed-binary smoke for atomic, explicit capture links.

Only uses fresh temporary stores and loopback HTTP. Builds/installs nothing.
"""

from __future__ import annotations

import argparse
import hashlib
import json
import os
from pathlib import Path
import selectors
import subprocess
import sys
import tempfile
import time

sys.dont_write_bytecode = True
sys.path.insert(0, str(Path(__file__).resolve().parents[1] / "integrations" / "codex"))
from mcp_client import McpClient, McpToolError, PROTOCOL_VERSION  # noqa: E402
from memory_library_smoke import Host  # noqa: E402


def need(ok: bool, why: str) -> None:
    if not ok:
        raise RuntimeError(why)


def artifact(path: Path) -> dict:
    digest = hashlib.sha256()
    with path.open("rb") as stream:
        for block in iter(lambda: stream.read(1024 * 1024), b""):
            digest.update(block)
    return {"path": str(path), "bytes": path.stat().st_size,
            "sha256": digest.hexdigest()}


def claim(key: str, *, links: list[dict] | None = None, **extra) -> dict:
    value = {"source": {"namespace": "installed-smoke", "key": key,
                        "reference": f"smoke://capture-links/{key}"},
             "summary": f"Explicit link smoke claim {key}", "body": f"Disposable evidence for {key}"}
    if links is not None:
        value["links"] = links
    value.update(extra)
    return value


def edge(node: dict, target: str) -> dict | None:
    return next((item for item in node.get("edges", []) if item.get("neighbor") == target), None)


def cli(binary: Path, root: Path, env: dict, db: Path, *args: str,
        payload: dict | None = None, ok: bool = True) -> dict | str:
    result = subprocess.run([str(binary), "--db", str(db), "--json", *args],
                            cwd=root, env=env,
                            input=json.dumps(payload) if payload is not None else None,
                            text=True, capture_output=True, timeout=90)
    need((result.returncode == 0) == ok,
         f"CLI {args!r} exit={result.returncode}: {result.stderr[-1000:]}")
    return json.loads(result.stdout) if ok else result.stderr


def remote_cli(binary: Path, root: Path, env: dict, url: str, payload: dict,
               *, ok: bool = True) -> dict | str:
    result = subprocess.run([str(binary), "--remote", url, "--remote-db", "project",
                             "--json", "capture", "add", "--input", "-"],
                            cwd=root, env=env, input=json.dumps(payload), text=True,
                            capture_output=True, timeout=90)
    need((result.returncode == 0) == ok,
         f"remote CLI exit={result.returncode}: {result.stderr[-1000:]}")
    return json.loads(result.stdout) if ok else result.stderr


class Stdio:
    def __init__(self, binary: Path, root: Path, env: dict, db: Path, profile: str):
        self.log = (root / f"{profile}-stdio.log").open("wb")
        self.process = subprocess.Popen(
            [str(binary), "--capability-profile", profile, "--db", f"project={db}"],
            cwd=root, env=env, stdin=subprocess.PIPE, stdout=subprocess.PIPE, stderr=self.log)
        self.next_id = 1
        self.buffer = b""
        try:
            reply = self.request("initialize", {"protocolVersion": PROTOCOL_VERSION,
                "capabilities": {}, "clientInfo": {"name": "capture-links-smoke", "version": "1"}})
            need(reply.get("protocolVersion") == PROTOCOL_VERSION, "stdio negotiation mismatch")
            self.request("notifications/initialized", {}, notification=True)
        except Exception:
            self.stop()
            raise

    def request(self, method: str, params: dict, *, notification: bool = False) -> dict | None:
        message = {"jsonrpc": "2.0", "method": method, "params": params}
        if not notification:
            message["id"] = self.next_id
            self.next_id += 1
        raw = (json.dumps(message, separators=(",", ":")) + "\n").encode()
        need(len(raw) <= 128 * 1024, "stdio request too large")
        need(self.process.poll() is None, "stdio host exited")
        self.process.stdin.write(raw)
        self.process.stdin.flush()
        if notification:
            return None
        deadline = time.monotonic() + 90
        with selectors.DefaultSelector() as selector:
            selector.register(self.process.stdout, selectors.EVENT_READ)
            while b"\n" not in self.buffer:
                remaining = deadline - time.monotonic()
                need(remaining > 0 and bool(selector.select(remaining)), f"stdio {method} timed out")
                chunk = os.read(self.process.stdout.fileno(), 65536)
                need(bool(chunk), f"stdio closed before {method} response")
                self.buffer += chunk
                need(len(self.buffer) <= 512 * 1024, "stdio frame too large")
        line, self.buffer = self.buffer.split(b"\n", 1)
        reply = json.loads(line)
        need(reply.get("id") == message["id"] and "error" not in reply,
             f"stdio {method} RPC refusal: {reply!r}")
        return reply["result"]

    def tool(self, name: str, arguments: dict, *, ok: bool = True) -> dict | str:
        result = self.request("tools/call", {"name": name, "arguments": arguments})
        need(result.get("isError") is (not ok), f"stdio {name} unexpected result: {result!r}")
        value = result["content"][0]["text"]
        return json.loads(value) if ok else value

    def stop(self) -> None:
        if self.process.poll() is None:
            self.process.stdin.close()
            try:
                self.process.wait(timeout=10)
            except subprocess.TimeoutExpired:
                self.process.terminate()
                try:
                    self.process.wait(timeout=5)
                except subprocess.TimeoutExpired:
                    self.process.kill()
                    self.process.wait(timeout=5)
        self.process.stdout.close()
        self.log.close()


def expect_http_error(client: McpClient, name: str, arguments: dict) -> str:
    try:
        client.call_tool(name, arguments)
    except McpToolError as error:
        return str(error)
    raise RuntimeError(f"HTTP {name} unexpectedly succeeded")


def run() -> None:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--mnemed", required=True, type=Path)
    parser.add_argument("--mcp", required=True, type=Path)
    parser.add_argument("--old-mnemed", type=Path,
                        help="capture-v1 installed CLI for detached upgrade and writer-fence check")
    parser.add_argument("--old-mcp", type=Path,
                        help="matching pre-links installed MCP for remote negotiation check")
    parser.add_argument("--output", required=True, type=Path)
    args = parser.parse_args()
    need((args.old_mnemed is None) == (args.old_mcp is None),
         "pass both old binaries, or neither")
    for binary in (args.mnemed, args.mcp, args.old_mnemed, args.old_mcp):
        if binary is None:
            continue
        need(binary.is_file() and os.access(binary, os.X_OK), f"missing executable: {binary}")
    mnemed, mcp, output = args.mnemed.resolve(), args.mcp.resolve(), args.output.resolve()
    os.environ["MNEME_CLIENT_BINARY"] = str(mnemed)
    old_mnemed = args.old_mnemed.resolve() if args.old_mnemed else None
    old_mcp = args.old_mcp.resolve() if args.old_mcp else None
    need(output not in (mnemed, mcp, old_mnemed, old_mcp), "receipt must not overwrite a binary")
    output.parent.mkdir(parents=True, exist_ok=True)
    receipt = {"artifacts": {"mnemed": artifact(mnemed), "mneme-mcp": artifact(mcp)},
               "checks": [], "ids": {}}
    if old_mnemed is not None:
        receipt["artifacts"].update({"old-mnemed": artifact(old_mnemed),
                                     "old-mneme-mcp": artifact(old_mcp)})
    with tempfile.TemporaryDirectory(prefix="mneme-capture-links-smoke-") as dirname:
        root = Path(dirname)
        env = os.environ.copy()
        env.pop("MNEME_DB", None)
        env["MNEME_RERANK"] = "0"
        env["GIT_CEILING_DIRECTORIES"] = str(root)
        cli_db, stdio_db, http_db = (root / f"{name}.db" for name in ("cli", "stdio", "http"))
        for db in (cli_db, stdio_db, http_db):
            initialized = cli(mnemed, root, env, db, "capture", "init")
            need(initialized.get("status") == "initialized", "capture init failed")
        # One-shot CLI, never competing with an MCP lease.
        target = cli(mnemed, root, env, cli_db, "capture", "add", "--input", "-",
                     payload=claim("cli-target"))
        linked = claim("cli-linked", links=[{"to": target["id"], "kind": "derived_from", "weight": 0.8}])
        made = cli(mnemed, root, env, cli_db, "capture", "add", "--input", "-", payload=linked)
        # Structural derived_from arrows traverse backward: read at the target.
        got = cli(mnemed, root, env, cli_db, "get", target["id"], "--edges")
        e = edge(got, made["id"])
        need(e is not None and e["incoming"] is True and e["kind"] == "derived_from" and abs(e["weight"] - 0.8) < 1e-5,
             f"CLI explicit edge absent or wrong: {got.get('edges')!r}")
        replay = cli(mnemed, root, env, cli_db, "capture", "add", "--input", "-", payload=linked)
        need(replay == {"id": made["id"], "replayed": True}, "CLI exact replay mismatch")
        missing = claim("cli-missing", links=[{"to": "00000000000000000000000000"}])
        cli(mnemed, root, env, cli_db, "capture", "add", "--input", "-", payload=missing, ok=False)
        unlinked = cli(mnemed, root, env, cli_db, "capture", "add", "--input", "-",
                       payload=claim("cli-missing"))
        need(unlinked["replayed"] is False, "failed CLI link published a capture identity")
        receipt["checks"].extend(["cli_link_readback", "cli_exact_replay", "cli_missing_target_atomicity"])
        receipt["ids"]["cli"] = {"target": target["id"], "linked": made["id"]}

        stdio = Stdio(mcp, root, env, stdio_db, "curator")
        try:
            s_target = stdio.tool("capture", {"db": "project", **claim("stdio-target")})
            s_claim = claim("stdio-linked", links=[{"to": s_target["id"], "kind": "transition", "weight": 0.6}])
            s_linked = stdio.tool("capture", {"db": "project", **s_claim})
            s_got = stdio.tool("get", {"db": "project", "id": s_linked["id"], "edges": True})
            se = edge(s_got, s_target["id"])
            need(se is not None and se["kind"] == "transition" and abs(se["weight"] - 0.6) < 1e-5,
                 f"stdio explicit edge absent or wrong: {s_got.get('edges')!r}")
            s_replay = stdio.tool("capture", {"db": "project", **s_claim})
            need(s_replay.get("id") == s_linked["id"] and s_replay.get("replayed") is True,
                 f"stdio replay mismatch: {s_replay!r}")
            need(s_got.get("status") == "active", "curator capture was not immediately Active")
            denial = stdio.tool("capture", {"db": "project", **claim("stdio-core", tags=["core"])}, ok=False)
            need("capability" in denial.lower() or "operator" in denial.lower(),
                 f"curator core refusal not authority based: {denial}")
            receipt["ids"]["stdio"] = {"target": s_target["id"], "linked": s_linked["id"]}
            receipt["checks"].extend(["stdio_curator_link_readback_replay",
                                      "stdio_curator_immediate_active_core_denied"])
        finally:
            stdio.stop()
        read_only = Stdio(mcp, root, env, stdio_db, "read-only")
        try:
            denial = read_only.tool("capture", {"db": "project", **claim("read-only-denied",
                links=[{"to": s_target["id"]}])}, ok=False)
            need("capability" in denial.lower(), f"read-only capture refusal not authority based: {denial}")
            receipt["checks"].append("stdio_read_only_denied")
        finally:
            read_only.stop()

        host = Host(mcp, root, "capture-http", ["--capability-profile", "operator",
                    "--db", f"project={http_db}"], env)
        try:
            with McpClient(host.url, timeout=30) as client:
                h_target = client.call_tool("capture", {"db": "project", **claim("http-target")})
                h_claim = claim("http-linked", links=[{"to": h_target["id"], "kind": "transition", "weight": 0.7}])
                h_linked = client.call_tool("capture", {"db": "project", **h_claim})
                prepared = client.prepare_capture(h_claim)
                need(prepared["id"] == h_linked["id"], "native prepare identity diverged")
                verified = client.capture_verified("project", h_claim)
                need(verified["id"] == h_linked["id"] and verified["replayed"] is True,
                     "native verified capture/readback failed")
                receipt["checks"].append("native_preflight_verified_capture_same_session")
                h_got = client.call_tool("get", {"db": "project", "id": h_linked["id"], "edges": True})
                he = edge(h_got, h_target["id"])
                need(he is not None and he["kind"] == "transition" and abs(he["weight"] - 0.7) < 1e-5,
                     f"HTTP explicit edge absent or wrong: {h_got.get('edges')!r}")
                changed = claim("http-linked", links=[{"to": h_target["id"], "kind": "transition", "weight": 0.8}])
                need("conflict" in expect_http_error(client, "capture", {"db": "project", **changed}).lower(),
                     "changed HTTP link request did not conflict")
                missing_h = claim("http-missing", links=[{"to": "00000000000000000000000000"}])
                expect_http_error(client, "capture", {"db": "project", **missing_h})
                h_after_failure = client.call_tool("capture", {"db": "project", **claim("http-missing")})
                need(h_after_failure["replayed"] is False, "failed HTTP link published capture")
                client.call_tool("forget", {"db": "project", "id": h_target["id"]})
                after_delete = client.call_tool("get", {"db": "project", "id": h_linked["id"], "edges": True})
                need(edge(after_delete, h_target["id"]) is None, "forget retained the edge")
                h_replay = client.call_tool("capture", {"db": "project", **h_claim})
                need(h_replay.get("id") == h_linked["id"] and h_replay.get("replayed") is True,
                     f"HTTP replay repaired deleted edge: {h_replay!r}")
                after_replay = client.call_tool("get", {"db": "project", "id": h_linked["id"], "edges": True})
                need(edge(after_replay, h_target["id"]) is None, "HTTP replay recreated deleted edge")
                receipt["ids"]["http"] = {"target": h_target["id"], "linked": h_linked["id"]}
                receipt["checks"].extend(["http_link_readback", "http_changed_links_conflict",
                    "http_missing_target_atomicity", "http_replay_does_not_repair_deleted_edge"])
        finally:
            host.stop()

        if old_mnemed is not None:
            skew_source = root / "skew-capture-v1.db"
            skew_db = root / "skew-single-graph.db"
            cli(old_mnemed, root, env, skew_source, "capture", "init")
            old_plain = claim("skew-plain")
            old_first = cli(old_mnemed, root, env, skew_source, "capture", "add", "--input", "-",
                            payload=old_plain)
            old_row = cli(old_mnemed, root, env, skew_source, "get", old_first["id"], "--body")
            old_source = old_row["provenance"]["source"]
            source_sha256 = artifact(skew_source)["sha256"]
            refusal = cli(mnemed, root, env, skew_source, "get", old_first["id"], ok=False)
            need("single-graph" in refusal.lower() or "single_graph" in refusal.lower(),
                 f"successor opened an unupgraded capture-v1 artifact: {refusal}")
            upgraded = cli(mnemed, root, env, skew_source, "single-graph-upgrade", "--backend", "sqlite",
                           "--output", str(skew_db))
            need(upgraded.get("status") == "upgraded_copy" and skew_db.is_file()
                 and artifact(skew_source)["sha256"] == source_sha256,
                 "detached upgrade failed or changed predecessor bytes")
            migrated_row = cli(mnemed, root, env, skew_db, "get", old_first["id"], "--body")
            migrated_source = migrated_row["provenance"]["source"]
            need(migrated_source.get("request_codec") == "capture_v1"
                 and {key: value for key, value in migrated_source.items() if key != "request_codec"}
                     == old_source
                 and all(migrated_row.get(field) == old_row.get(field) for field in ("summary", "body")),
                 "detached upgrade changed the original v1 capture proof or body")
            new_replay = cli(mnemed, root, env, skew_db, "capture", "add", "--input", "-",
                             payload=old_plain)
            need(new_replay.get("id") == old_first["id"] and new_replay.get("replayed") is True,
                 "new CLI failed to replay old unlinked request")
            skew_linked = claim("skew-linked", links=[{"to": old_first["id"],
                                   "kind": "transition", "weight": 0.75}])
            new_linked = cli(mnemed, root, env, skew_db, "capture", "add", "--input", "-",
                             payload=skew_linked)
            new_got = cli(mnemed, root, env, skew_db, "get", new_linked["id"], "--edges")
            new_edge = edge(new_got, old_first["id"])
            need(new_edge is not None and new_edge["kind"] == "transition",
                 f"upgraded store lost the new capture edge: {new_got.get('edges')!r}")
            successor_sha256 = artifact(skew_db)["sha256"]
            old_refusal = cli(old_mnemed, root, env, skew_db, "capture", "add", "--input", "-",
                              payload=claim("old-must-not-write"), ok=False)
            need("generation" in old_refusal.lower() or "catalog" in old_refusal.lower()
                 or "unsupported" in old_refusal.lower(),
                 f"old writer did not refuse successor: {old_refusal}")
            need(artifact(skew_db)["sha256"] == successor_sha256,
                 "refused old writer changed successor bytes")
            receipt["ids"]["skew"] = {"old_unlinked": old_first["id"], "new_linked": new_linked["id"]}
            receipt["checks"].extend(["old_capture_artifact_refused", "detached_single_graph_upgrade",
                                      "old_unlinked_new_replay", "new_link_readback",
                                      "old_writer_successor_refusal"])
            old_host = Host(old_mcp, root, "old-capture-http",
                            ["--capability-profile", "operator", "--db", f"project={skew_source}"], env)
            try:
                remote_linked = claim("remote-skew", links=[{"to": old_first["id"]}])
                refusal = remote_cli(mnemed, root, env, old_host.url, remote_linked, ok=False)
                need("links are unavailable" in refusal,
                     f"new CLI did not reject old remote links before mutation: {refusal}")
                plain = remote_cli(mnemed, root, env, old_host.url, claim("remote-skew"))
                need(plain.get("replayed") is False,
                     "old remote host received linked request before new CLI refusal")
                receipt["checks"].extend(["new_cli_old_mcp_link_refusal",
                                           "old_mcp_unlinked_capture_after_refusal"])
            finally:
                old_host.stop()
    output.write_text(json.dumps(receipt, sort_keys=True, indent=2) + "\n", encoding="utf-8")
    print(json.dumps({"receipt": str(output), "checks": receipt["checks"]}, sort_keys=True))


if __name__ == "__main__":
    try:
        run()
    except Exception as error:
        print(f"capture_links_smoke: {error}", file=sys.stderr)
        raise
