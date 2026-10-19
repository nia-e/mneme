#!/usr/bin/env python3
"""Finite installed-replica smoke; synthetic SQLite snapshots, offline by default.

The fixture CLI is a separate, same-source testTools output, not a replica runtime
dependency. It migrates deliberately synthetic v6 JSON through the native current
store importer. The vector is a unit vector, NOT a BGE inference result: its BGE
fingerprint tests admission compatibility only, never retrieval quality. A native
snapshot is then copied to a disposable read-only MCP replica. No enrolled owner
is opened. Optional --model-cache copies only explicit BGE assets and exercises
real inference, but requires a loopback-only Linux network namespace.
"""

from __future__ import annotations

import argparse
import hashlib
import http.client
import json
import os
from pathlib import Path
import re
import selectors
import shutil
import socket
import subprocess
import sys
import tempfile
import time


DB = "01ARZ3NDEKTSV4RRFFQ69G5FA1"
NODE = "01ARZ3NDEKTSV4RRFFQ69G5FA2"
OTHER_DB = "01ARZ3NDEKTSV4RRFFQ69G5FA3"
SUMMARY = "Synthetic offline replica fixture; not an inferred memory."
BODY = "Only disposable fixture content. No inference was used.\n"
PROTOCOL = "2025-11-25"
TIMEOUT = 30
INFERENCE_TIMEOUT = 120
MAX_RESPONSE = 512 * 1024
BGE = {
    "format_version": 1,
    "embedding_id": "fastembed:Xenova/bge-base-en-v1.5:onnx-fp32:mneme-adapter-v1",
    "dimension": 768,
    "normalization": "l2-f32-v1",
    "query_mode": "bge-v1.5-search-instruction-v1",
}
READ_ONLY_TOOLS = {
    "databases", "discovery_catalog", "activity", "database_control", "status", "query",
    "recall_context", "recall", "get", "list", "graph", "neighbors", "remote_edges",
    "core", "contradictions", "merges", "walk", "episode", "concern",
}


def require(condition, message):
    if not condition:
        raise RuntimeError(message)


def digest(path):
    value = hashlib.sha256()
    with path.open("rb") as stream:
        for chunk in iter(lambda: stream.read(1024 * 1024), b""):
            value.update(chunk)
    return value.hexdigest()


def fixture(fingerprint):
    return {
        "schema": "mneme.store-export.v6",
        "tag_normalization_version": "ascii-slug-v1",
        "store": {
            "db_id": DB, "dim": 768, "embedding_fingerprint": fingerprint,
            "nodes": [{
                "id": NODE, "summary": SUMMARY, "body": "fs://smoke.txt",
                "tags": ["nix-smoke"], "provenance": {"Derived": {"from": []}},
                "stability": 0.5, "confidence": 0.5,
                "created": 1700000000000, "status": "Active",
            }],
            "vectors": [[NODE, [1.0] + [0.0] * 767]],
            "edges": [], "contradictions": [], "concerns": [], "touchstones": [],
        },
    }


def isolated_env(root):
    # Do not inherit credentials, owner paths, hooks, model caches or user config.
    env = {"PATH": os.environ.get("PATH", ""), "LANG": "C", "TMPDIR": str(root),
           "HOME": str(root / "home"), "XDG_CONFIG_HOME": str(root / "config"),
           "XDG_DATA_HOME": str(root / "data"), "XDG_CACHE_HOME": str(root / "cache"),
           "HF_HOME": str(root / "models"), "FASTEMBED_CACHE_DIR": str(root / "models"),
           "HF_HUB_OFFLINE": "1", "HF_ENDPOINT": "http://127.0.0.1:1", "MNEME_RERANK": "0",
           # Defence in depth if a regression accidentally requests inference.
           # This is not an OS network sandbox or a cached-model qualification.
           "HTTP_PROXY": "http://127.0.0.1:1", "HTTPS_PROXY": "http://127.0.0.1:1",
           "ALL_PROXY": "http://127.0.0.1:1", "NO_PROXY": "127.0.0.1,localhost"}
    for name in ("home", "config", "data", "cache", "models"):
        (root / name).mkdir()
    return env


def run(command, env, cwd, *, success=True, timeout=TIMEOUT):
    result = subprocess.run(command, env=env, cwd=cwd, stdin=subprocess.DEVNULL,
                            stdout=subprocess.PIPE, stderr=subprocess.PIPE,
                            text=True, timeout=timeout)
    require((result.returncode == 0) == success,
            f"unexpected exit {result.returncode}: {command!r}\n"
            f"{result.stdout[-8192:]}\n{result.stderr[-8192:]}")
    return result


def installed_metadata(prefix, binaries, features):
    metadata = json.loads((prefix / "share/mneme/build-info.json").read_text())
    require(metadata.get("schema") == "mneme.nix-package.v1", "unknown package metadata")
    require(metadata.get("defaultFeatures") is False, "implicit default features in package")
    require(sorted(metadata.get("features", [])) == sorted(features),
            f"wrong package features: {metadata.get('features')!r}")
    require(sorted(metadata.get("binaries", [])) == sorted(binaries), "wrong package binaries")
    require(metadata.get("version") and metadata.get("sourcePath"), "missing package provenance")
    require(re.fullmatch(r"[0-9a-f]{64}", metadata.get("cargoLockSha256", "")),
            "invalid package Cargo.lock hash")
    expected_paths = {f"bin/{name}" for name in binaries}
    actual_paths = set()
    for line in (prefix / "share/mneme/binary-sha256.txt").read_text().splitlines():
        expected, name = line.split()
        require(name in expected_paths and name not in actual_paths,
                f"unexpected/duplicate package hash path: {name!r}")
        require(re.fullmatch(r"[0-9a-f]{64}", expected) and digest(prefix / name) == expected,
                f"installed hash mismatch for {name}")
        actual_paths.add(name)
    require(actual_paths == expected_paths, "incomplete package hash inventory")
    require({p.name for p in (prefix / "bin").iterdir()} == set(binaries),
            "unexpected executable in installed output")
    return metadata


class Process:
    def __init__(self, command, env, cwd, *, stdio=False):
        self.request_timeout = TIMEOUT
        self.err = tempfile.TemporaryFile()
        self.out = None if stdio else tempfile.TemporaryFile()
        try:
            self.process = subprocess.Popen(command, env=env, cwd=cwd,
                                            stdin=subprocess.PIPE if stdio else subprocess.DEVNULL,
                                            stdout=subprocess.PIPE if stdio else self.out,
                                            stderr=self.err)
        except BaseException:
            self.err.close()
            if self.out is not None:
                self.out.close()
            raise

    def diagnostics(self):
        self.err.seek(0, os.SEEK_END)
        self.err.seek(max(0, self.err.tell() - 8192))
        return self.err.read().decode(errors="replace")

    def __enter__(self):
        return self

    def __exit__(self, *_):
        if self.process.poll() is None:
            self.process.kill()
        self.process.wait(timeout=5)
        for stream in (self.process.stdin, self.process.stdout, self.out, self.err):
            if stream is not None:
                stream.close()

    def wait_success(self):
        status = self.process.wait(timeout=TIMEOUT)
        require(status == 0, f"server exit {status}: {self.diagnostics()}")


def rpc_result(response, request_id):
    require(response.get("jsonrpc") == "2.0" and response.get("id") == request_id,
            f"unexpected RPC response: {response!r}")
    require("error" not in response, f"RPC error: {response!r}")
    return response["result"]


class Stdio(Process):
    def __init__(self, command, env, cwd):
        super().__init__(command, env, cwd, stdio=True)
        self.pending = b""
        self.request_id = 0

    def notify(self, method, params):
        self.send({"jsonrpc": "2.0", "method": method, "params": params})

    def send(self, message):
        self.process.stdin.write(json.dumps(message).encode() + b"\n")
        self.process.stdin.flush()

    def request(self, method, params):
        self.request_id += 1
        self.send({"jsonrpc": "2.0", "id": self.request_id, "method": method, "params": params})
        deadline = time.monotonic() + self.request_timeout
        with selectors.DefaultSelector() as selector:
            selector.register(self.process.stdout, selectors.EVENT_READ)
            while b"\n" not in self.pending:
                remaining = deadline - time.monotonic()
                require(remaining > 0 and selector.select(remaining),
                        f"stdio response timed out: {self.diagnostics()}")
                chunk = os.read(self.process.stdout.fileno(), 65536)
                require(chunk, f"stdio EOF before response: {self.diagnostics()}")
                self.pending += chunk
                require(len(self.pending) <= MAX_RESPONSE, "stdio response too large")
        line, self.pending = self.pending.split(b"\n", 1)
        return rpc_result(json.loads(line), self.request_id)

    def finish(self):
        self.process.stdin.close()  # MCP stdio shutdown is EOF, not an RPC.
        self.wait_success()


class Http(Process):
    def __init__(self, command, env, cwd):
        # Reserve a loopback ephemeral port, then fail rather than rebind elsewhere
        # if another process takes it in the small handoff window.
        with socket.socket() as reservation:
            reservation.bind(("127.0.0.1", 0))
            self.port = reservation.getsockname()[1]
        super().__init__(command + ["--http", f"127.0.0.1:{self.port}"], env, cwd)
        self.session = None
        self.request_id = 0

    def exchange(self, method, message=None):
        headers = {"Content-Type": "application/json", "Accept": "application/json, text/event-stream"}
        if self.session:
            headers.update({"Mcp-Session-Id": self.session, "MCP-Protocol-Version": PROTOCOL})
        connection = http.client.HTTPConnection("127.0.0.1", self.port, timeout=self.request_timeout)
        try:
            connection.request(method, "/", body=None if message is None else json.dumps(message),
                               headers=headers)
            response = connection.getresponse()
            body = response.read(MAX_RESPONSE + 1)
            require(len(body) <= MAX_RESPONSE, "HTTP response too large")
            require(response.status in (200, 202, 204),
                    f"HTTP status {response.status}: {body[:8192]!r}")
            if self.session is None:
                self.session = response.getheader("Mcp-Session-Id")
            return None if not body else json.loads(body)
        finally:
            connection.close()

    def request(self, method, params):
        self.request_id += 1
        message = {"jsonrpc": "2.0", "id": self.request_id, "method": method, "params": params}
        deadline = time.monotonic() + TIMEOUT
        while True:
            require(self.process.poll() is None, f"HTTP server stopped: {self.diagnostics()}")
            try:
                response = self.exchange("POST", message)
                break
            except ConnectionRefusedError:
                # Retry only initial connection refusal, never an accepted call.
                require(method == "initialize" and time.monotonic() < deadline,
                        f"HTTP listener unavailable: {self.diagnostics()}")
                time.sleep(0.05)
        require(self.session, "HTTP initialize did not establish a session")
        return rpc_result(response, self.request_id)

    def notify(self, method, params):
        require(self.exchange("POST", {"jsonrpc": "2.0", "method": method, "params": params}) is None,
                "HTTP notification unexpectedly returned content")

    def finish(self):
        self.exchange("DELETE")
        self.process.terminate()
        self.wait_success()
        require("all databases cleanly released" in self.diagnostics(),
                "HTTP shutdown did not confirm database release")


def initialize(rpc, version, profile="read-only"):
    value = rpc.request("initialize", {"protocolVersion": PROTOCOL, "capabilities": {},
                                      "clientInfo": {"name": "nix-replica-smoke", "version": "1"}})
    require(value["protocolVersion"] == PROTOCOL and value["capabilityProfile"] == profile,
            f"wrong protocol/profile: {value!r}")
    require(value["serverInfo"]["version"] == version, "MCP/package version mismatch")
    rpc.notify("notifications/initialized", {})


def tool(rpc, name, arguments, *, error=None):
    result = rpc.request("tools/call", {"name": name, "arguments": arguments})
    require(result.get("isError") is (error is not None), f"wrong tool status: {result!r}")
    blocks = result["content"]
    require(len(blocks) == 1 and blocks[0]["type"] == "text", "wrong MCP content shape")
    text = blocks[0]["text"]
    if error is not None:
        require(error in text, f"wrong tool refusal: {text!r}")
        return text
    return json.loads(text)


def command_for(mcp, db, profile="read-only"):
    return [str(mcp), "--db", f"replica={db}", "--capability-profile", profile]


def migrate_fixture(cli, directory, env, *, fingerprint=BGE):
    directory.mkdir()
    db = directory / "memory.db"
    db.write_text(json.dumps(fixture(fingerprint)))
    db.with_suffix(".bodies").mkdir()
    (db.with_suffix(".bodies") / "smoke.txt").write_text(BODY)
    run([str(cli), "--db", str(db), "migrate"], env, directory)
    with db.open("rb") as stream:
        require(stream.read(16) == b"SQLite format 3\0", "fixture did not become SQLite")
    require(db.with_suffix(".snapshot.bak").is_file(), "native migration backup missing")
    return db


def snapshot_replica(mcp, source, root, env, version, *, db_id=DB,
                     files=("database.db", "bodies/smoke.txt")):
    with Stdio(command_for(mcp, source, "operator"), env, root) as rpc:
        initialize(rpc, version, "operator")
        result = tool(rpc, "snapshot_create", {"db": "replica", "expected_db_id": db_id})
        rpc.finish()
    bundle = Path(result["bundle"]).resolve()
    require(bundle.is_relative_to(root) and result["db_id"] == db_id, "snapshot escaped fixture root")
    manifest = json.loads((bundle / "manifest.json").read_text())
    require(manifest["schema"] == "mneme.snapshot.v1" and manifest["db_id"] == db_id
            and manifest["generation"] == result["generation"]
            and manifest["embedding_fingerprint"] == BGE, "snapshot identity/fingerprint mismatch")
    require({entry["path"] for entry in manifest["files"]} == set(files)
            and len(manifest["files"]) == len(files), "unexpected snapshot inventory")
    for entry in manifest["files"]:
        path = bundle / entry["path"]
        require(path.stat().st_size == entry["size"] and digest(path) == entry["sha256"],
                f"snapshot file mismatch: {entry['path']}")
    replica = root / "replica"
    replica.mkdir()
    db = replica / "memory.db"
    shutil.copyfile(bundle / "database.db", db)
    shutil.copytree(bundle / "bodies", db.with_suffix(".bodies"))
    return db, result["generation"]


def read_smoke(rpc, mcp, db, env, root, version):
    initialize(rpc, version)
    catalog = rpc.request("tools/list", {})
    names = {item["name"] for item in catalog["tools"]}
    require(names == READ_ONLY_TOOLS and len(catalog["tools"]) == len(names),
            f"wrong read-only catalog: {names!r}")
    schemas = {item["name"]: item["inputSchema"] for item in catalog["tools"]}
    for name, actions in (("database_control", {"status"}), ("concern", {"list"}),
                          ("episode", {"list", "search", "get", "history", "references"})):
        schema = schemas[name]
        require(set(schema["properties"]["action"]["enum"]) == actions,
                f"wrong read-only {name} action catalog")
        if "oneOf" in schema:
            require({branch["properties"]["action"]["const"] for branch in schema["oneOf"]} == actions,
                    f"wrong read-only {name} action variants")
    databases = tool(rpc, "databases", {})
    require(len(databases) == 1 and databases[0]["db"] == "replica"
            and databases[0]["db_id"] == DB and databases[0]["state"] == "open"
            and Path(databases[0]["resolved_path"]) == db, "wrong replica identity/path")
    held = run(command_for(mcp, db), env, root, success=False)
    require("already owned by another mneme process" in held.stderr, "held lease was not refused")
    args = {"db": "replica", "expected_db_id": DB}
    listed = tool(rpc, "list", {**args, "limit": 1})
    require(listed["db_id"] == DB and [item["id"] for item in listed["items"]] == [NODE]
            and listed["next_cursor"] is None and listed["has_more"] is False, "wrong replica list")
    got = tool(rpc, "get", {**args, "id": NODE, "body": True, "max_body_bytes": 1024})
    require(got["db_id"] == DB and got["id"] == NODE and got["summary"] == SUMMARY
            and got["body"] == BODY, "wrong replica get/body")
    status = tool(rpc, "status", args)
    require(status["nodes"] == 1 and status["active"] == 1, "wrong replica status")
    denial = tool(rpc, "forget", {**args, "id": NODE}, error="capability profile")
    tool(rpc, "database_control", {**args, "action": "release"}, error="capability profile")
    tool(rpc, "list", {**args, "limit": 65}, error="1..=64")
    tool(rpc, "get", {**args, "id": NODE, "expected_db_id": OTHER_DB}, error="expected_db_id mismatch")
    after = tool(rpc, "get", {**args, "id": NODE, "body": True, "max_body_bytes": 1024})
    require(after == got, "refused mutation changed the fixture")
    return {"catalog": catalog, "list": listed, "get": got, "status": status, "denial": denial}


def copy_model_cache(source, target):
    """Copy only the five BGE assets, never HF tokens or arbitrary cache contents."""
    require(sys.platform.startswith("linux") and {name for _, name in socket.if_nameindex()} == {"lo"},
            "--model-cache requires an isolated Linux network namespace with only loopback; "
            "run under unshare --user --map-root-user --net with lo brought up")
    source = source.resolve()
    name = "models--Xenova--bge-base-en-v1.5"
    repository = (source / name).resolve()
    require(repository.is_relative_to(source), "model repository leaves the supplied cache")
    revision = (repository / "refs/main").read_text().strip()
    require(re.fullmatch(r"[0-9a-f]{40}", revision), "model cache has no pinned 40-hex main revision")
    (target / name / "refs").mkdir(parents=True)
    (target / name / "refs/main").write_text(revision + "\n")
    hashes = {}
    for relative in ("onnx/model.onnx", "tokenizer.json", "config.json",
                     "special_tokens_map.json", "tokenizer_config.json"):
        original = (repository / "snapshots" / revision / relative).resolve()
        require(original.is_relative_to(repository) and original.is_file(),
                f"missing or escaping cached BGE asset: {relative}")
        require(0 < original.stat().st_size <= 512 * 1024 * 1024, f"unbounded BGE asset: {relative}")
        destination = target / name / "snapshots" / revision / relative
        destination.parent.mkdir(parents=True, exist_ok=True)
        shutil.copyfile(original, destination)
        hashes[relative] = digest(destination)
        require(hashes[relative] == digest(original), f"cached BGE asset changed while copying: {relative}")
    return {"revision": revision, "sha256": hashes}


def semantic_smoke(mcp, cli, cache, root, env, version):
    model = copy_model_cache(cache, root / "models")
    semantic = root / "semantic"
    semantic.mkdir()
    source = semantic / "source"
    source.mkdir()
    db = source / "memory.db"
    run([str(cli), "--json", "--db", str(db), "capture", "init"], env, semantic)
    inspected = json.loads(run([str(cli), "--json", "--db", str(db), "capture", "inspect"],
                               env, semantic).stdout)
    db_id = inspected["db_id"]
    summary = "A copper kettle whistles when steam from boiling water passes through its spout."
    packet = source / "capture.json"
    packet.write_text(json.dumps({"source": {"namespace": "nix-smoke", "key": "kettle",
                                             "reference": "fixture://nix-replica-smoke/kettle"},
                                  "summary": summary, "body": summary, "tags": ["nix-smoke"]}))
    captured = json.loads(run([str(cli), "--json", "--db", str(db), "capture", "add",
                               "--input", str(packet)], env, semantic,
                              timeout=INFERENCE_TIMEOUT).stdout)
    require(captured["replayed"] is False, "semantic fixture unexpectedly replayed a capture")
    node_id = captured["id"]
    body_files = list(db.with_suffix(".bodies").iterdir())
    require(len(body_files) == 1 and body_files[0].is_file(), "unexpected captured body inventory")
    replica, generation = snapshot_replica(mcp, db, semantic, env, version, db_id=db_id,
                                          files=("database.db", f"bodies/{body_files[0].name}"))
    for transport in (Stdio, Http):
        with transport(command_for(mcp, replica), env, semantic) as rpc:
            rpc.request_timeout = INFERENCE_TIMEOUT
            initialize(rpc, version)
            query = tool(rpc, "query", {"db": "replica", "expected_db_id": db_id,
                                        "text": "Why does a copper kettle whistle?",
                                        "k": 1, "depth": 0, "max_nodes": 1, "min_relevance": 0.0})
            require(query["schema"] == "mneme.query.v3"
                    and [hit["id"] for hit in query["lanes"]["primary"]["hits"]] == [node_id],
                    "cached BGE query did not retrieve the independently embedded fixture")
            got = tool(rpc, "get", {"db": "replica", "expected_db_id": db_id, "id": node_id})
            require(got["summary"] == summary, "semantic replica returned wrong fixture")
            rpc.finish()
        inspected = json.loads(run([str(cli), "--json", "--db", str(replica), "capture", "inspect"],
                                   env, semantic).stdout)
        require(inspected["db_id"] == db_id, "semantic replica lease/identity readback failed")
    return {"model": model, "transports": ["stdio", "http"], "node_id": node_id,
            "snapshot_generation": generation, "network": "loopback-only Linux namespace",
            "boundary": "one embedded capture and one query per transport; not retrieval-quality evaluation"}


def smoke(prefix, cli, client_prefix, root, model_cache=None):
    env = isolated_env(root)
    mcp = prefix / "bin/mneme-mcp"
    metadata = installed_metadata(prefix, ["mneme-mcp"], ["cozo", "fastembed", "http"])
    helper = installed_metadata(cli.parent.parent, ["mnemed"], ["cozo", "fastembed"])
    for key in ("version", "sourceRevision", "sourcePath", "cargoLockSha256"):
        require(metadata.get(key) == helper.get(key), f"fixture CLI/package {key} mismatch")
    require(run([str(cli), "--version"], env, root).stdout.strip() == f"mnemed {metadata['version']}",
            "fixture CLI/package version mismatch")
    help_text = run([str(mcp), "--help"], env, root, success=False).stderr
    require("--http" in help_text and "--capability-profile" in help_text, "MCP help missing replica options")
    source = migrate_fixture(cli, root / "source", env)
    db, generation = snapshot_replica(mcp, source, root, env, metadata["version"])
    results = []
    for transport in (Stdio, Http):
        with transport(command_for(mcp, db), env, root) as rpc:
            results.append(read_smoke(rpc, mcp, db, env, root, metadata["version"]))
            rpc.finish()
        # A new native process must reacquire the exact lease after shutdown.
        inspected = json.loads(run([str(cli), "--json", "--db", str(db), "capture", "inspect"],
                                   env, root).stdout)
        require(inspected["status"] == "verified" and inspected["db_id"] == DB,
                "post-shutdown native lease/identity verification failed")
    require(results[0] == results[1], "stdio/HTTP read, catalog or denial parity mismatch")

    mismatch = dict(BGE, embedding_id="nix-smoke:deliberately-incompatible")
    wrong = migrate_fixture(cli, root / "mismatch", env, fingerprint=mismatch)
    refused = run(command_for(mcp, wrong), env, root, success=False)
    require("embedding fingerprint mismatch" in refused.stderr and "reembed" in refused.stderr,
            "incompatible fingerprint was not clearly refused")
    broken_dir = root / "broken"
    broken_dir.mkdir()
    broken = broken_dir / "memory.db"
    broken.write_bytes(b"deliberately not a database\n")
    before = digest(broken)
    run(command_for(mcp, broken), env, root, success=False)
    require(digest(broken) == before, "broken database was replaced or changed")
    if client_prefix is not None:
        installed_metadata(client_prefix, ["mnemed", "mneme-mcp"], [])
        refused = run([str(client_prefix / "bin/mnemed"), "--db", str(db), "capture", "inspect"],
                      env, root, success=False)
        require("cozo-enabled" in refused.stderr, "client did not explain its absent persistent backend")
        client_db = root / "client-http" / "memory.json"
        client_db.parent.mkdir()
        refused = run(command_for(client_prefix / "bin/mneme-mcp", client_db) + ["--http", "127.0.0.1:0"],
                      env, root, success=False)
        require("--http needs the `http` feature" in refused.stderr, "client HTTP feature refusal missing")
    require(not list((root / "models").rglob("*")), "default smoke accessed/downloaded model cache")
    result = {"package": str(prefix), "source_revision": metadata.get("sourceRevision"),
            "source_path": metadata["sourcePath"], "mcp_sha256": digest(mcp),
            "fixture_cli_sha256": digest(cli), "snapshot_generation": generation,
            "transports": ["stdio", "http"], "nodes": 1, "model_inference": "not exercised",
            "client_feature_boundary": "passed" if client_prefix is not None else "not requested"}
    if model_cache is not None:
        result["model_inference"] = semantic_smoke(mcp, cli, model_cache, root, env, metadata["version"])
    return result


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("prefix", type=Path, help="installed replica output containing bin/mneme-mcp")
    parser.add_argument("--fixture-cli", type=Path, required=True,
                        help="same-source testTools output's bin/mnemed (cozo+fastembed)")
    parser.add_argument("--client-prefix", type=Path,
                        help="optional no-default client output for missing-feature refusal checks")
    parser.add_argument("--model-cache", type=Path,
                        help="optional existing BGE cache; requires Linux network namespace with only lo; "
                             "copies only pinned model assets and tests real capture/query inference")
    args = parser.parse_args()
    with tempfile.TemporaryDirectory(prefix="mneme-replica-smoke-") as scratch:
        result = smoke(args.prefix.resolve(), args.fixture_cli.resolve(),
                       args.client_prefix.resolve() if args.client_prefix else None,
                       Path(scratch).resolve(), args.model_cache)
    print("replica smoke passed: " + json.dumps(result, sort_keys=True))


if __name__ == "__main__":
    main()
