#!/usr/bin/env python3
"""Finite CLI/default-owner + native MCP HTTP smoke over disposable current stores.

Requires a matched CLI/MCP pair with persistent backend and no network-dependent
model inputs (e.g. --no-default-features --features cozo,http), or a complete
--embedding-cache copied into scratch. Does not build, install,
enroll real projects, change services, or open any existing memory store. All
HOME/XDG/config/store paths belong to a TemporaryDirectory. Receipts contain
command labels, identities, sizes and hashes, never memory bodies or raw output.
"""
from __future__ import annotations

import argparse
import hashlib
import json
import os
from pathlib import Path
import selectors
import re
import signal
import socket
import subprocess
import sys
import tempfile
import time

sys.dont_write_bytecode = True
sys.path.insert(0, str(Path(__file__).resolve().parents[1] / "integrations/codex"))
from mcp_client import McpClient, McpError, McpTransportError  # noqa: E402

MAX_OUTPUT = 2 * 1024 * 1024
COLD_TOOLS = {"status", "core", "forget", "contradictions", "merges", "decay", "prune", "snapshot_create"}


def need(condition, message):
    if not condition:
        raise RuntimeError(message)


def digest(value):
    return hashlib.sha256(value).hexdigest()


def artifact(path):
    hashed = hashlib.sha256()
    with path.open("rb") as stream:
        for chunk in iter(lambda: stream.read(1024 * 1024), b""):
            hashed.update(chunk)
    return {"path": str(path), "bytes": path.stat().st_size, "sha256": hashed.hexdigest()}


def encode(value):
    return json.dumps(value, sort_keys=True, separators=(",", ":"), ensure_ascii=False).encode()


def write_json(path, value):
    path.parent.mkdir(parents=True, exist_ok=True)
    path.write_bytes(encode(value) + b"\n")


def tree_hash(root):
    """Small fixture-only tree including directories; never follows symlinks."""
    entries = []
    for path in sorted(root.rglob("*")):
        relative = str(path.relative_to(root))
        if path.is_symlink():
            entries.append((relative, "symlink", os.readlink(path)))
        elif path.is_file():
            entries.append((relative, "file", digest(path.read_bytes())))
        else:
            entries.append((relative, "directory"))
    return digest(encode(entries))


class Smoke:
    def __init__(self, options, root, receipt):
        self.options, self.root, self.receipt = options, root.resolve(), receipt
        root = self.root
        self.deadline = time.monotonic() + options.timeout
        self.last_cold = 0.0
        self.children = []
        self.clients = []
        self.logs = []
        self.env = {key: value for key, value in os.environ.items()
                    if not key.startswith(("MNEME_", "XDG_", "HF_", "HUGGINGFACE_"))}
        self.env.update({"HOME": str(root / "home"), "XDG_DATA_HOME": str(root / "data"),
                         "XDG_CONFIG_HOME": str(root / "config"), "XDG_CACHE_HOME": str(root / "cache"),
                         "XDG_STATE_HOME": str(root / "state"),
                         "HF_HOME": str(root / "cache/hf"), "HF_HUB_OFFLINE": "1",
                         "TRANSFORMERS_OFFLINE": "1", "HF_ENDPOINT": "http://127.0.0.1:9", "MNEME_RERANK": "0",
                         "MNEME_CLIENT_BINARY": str(options.cli), "GIT_CEILING_DIRECTORIES": str(root),
                         "HTTP_PROXY": "http://127.0.0.1:9", "HTTPS_PROXY": "http://127.0.0.1:9",
                         "ALL_PROXY": "http://127.0.0.1:9", "NO_PROXY": "127.0.0.1,localhost,::1"})
        # The integration adapter starts its child from os.environ. Keep the same
        # isolated environment for that route, rather than inheriting live config.
        for key in ("http_proxy", "https_proxy", "all_proxy", "no_proxy"):
            self.env[key] = self.env[key.upper()]
        os.environ.clear()
        os.environ.update(self.env)
        self.project = root / "project"
        self.cwd = self.project / "nested"
        self.blank = root / "unenrolled"
        for path in (self.cwd, self.blank, root / "stores", root / "home", root / "data"):
            path.mkdir(parents=True, exist_ok=True)
        self.db = {name: root / "stores" / (name + ".db") for name in ("project", "user")}
        self.enrollment = {"project": self.project / ".mneme/cli.json",
                           "user": root / "config/mneme/cli.json"}
        self.ids = {}
        self.sentinels = {}

    def prepare_cache(self):
        """Copy only the default embedder's required assets, never lend a live cache.

        Rust hf-hub ignores HF_HUB_OFFLINE. HF_ENDPOINT is separately pinned to
        loopback discard, so an incomplete artifact/cache cannot download models.
        """
        source = getattr(self.options, "embedding_cache", None)
        if source is None:
            self.receipt["model_inputs"] = "none; intended for model-independent hashing builds"
            return
        source = source.resolve()
        repository = "models--Xenova--bge-base-en-v1.5"
        reference = source / repository / "refs/main"
        need(reference.is_file() and reference.resolve().is_relative_to(source), "embedding cache ref is missing or escapes the source cache")
        with reference.open("rb") as stream:
            revision = stream.read(129)
        need(re.fullmatch(rb"[0-9a-f]{40}", revision) is not None, "embedding cache needs an exact BGE-base revision")
        files = [Path(repository) / "snapshots" / revision.decode() / name for name in
                 ("onnx/model.onnx", "tokenizer.json", "config.json", "special_tokens_map.json", "tokenizer_config.json")]
        for relative in files:
            path = source / relative
            need(path.is_file() and path.stat().st_size > 0 and path.resolve().is_relative_to(source),
                 "embedding cache asset is missing, empty or escapes the source cache")
        target = self.root / "cache/hf"
        copied_ref = target / repository / "refs/main"
        copied_ref.parent.mkdir(parents=True, exist_ok=True)
        copied_ref.write_bytes(revision)
        records = []
        for relative in files:
            path, copied = source / relative, target / relative
            copied.parent.mkdir(parents=True, exist_ok=True)
            hashed = hashlib.sha256()
            with path.open("rb") as reader, copied.open("xb") as writer:
                for block in iter(lambda: reader.read(1024 * 1024), b""):
                    self.remaining()
                    writer.write(block)
                    hashed.update(block)
            records.append({"path": str(relative), "bytes": copied.stat().st_size, "sha256": hashed.hexdigest()})
        self.receipt["model_inputs"] = {"source_cache": str(source), "disposable_cache": str(target),
            "revision": revision.decode(), "assets": records, "network_endpoint": self.env["HF_ENDPOINT"]}

    def remaining(self):
        remaining = self.deadline - time.monotonic()
        need(remaining > 0, "smoke total operation deadline exceeded")
        return min(30.0, remaining)

    def pace(self, tool, args=None):
        if tool not in COLD_TOOLS and not (tool == "database_control" and (args or {}).get("action") != "status"):
            return
        delay = max(0.0, self.last_cold + 1.05 - time.monotonic())
        need(delay < self.remaining(), "deadline exhausted before cold operation")
        time.sleep(delay)
        self.last_cold = time.monotonic()

    def command(self, label, argv, *, cwd=None, stdin=None, ok=True, mode="json", executable=None):
        start = time.monotonic()
        timeout = self.remaining()
        child = subprocess.Popen([str(executable or self.options.cli), *map(str, argv)], cwd=cwd or self.cwd,
                                 env=self.env, stdin=subprocess.PIPE if stdin is not None else subprocess.DEVNULL,
                                 stdout=subprocess.PIPE, stderr=subprocess.PIPE, bufsize=0,
                                 start_new_session=executable is not None)
        child._smoke_process_group = executable is not None
        self.children.append(child)
        out, err = bytearray(), bytearray()
        incoming = memoryview(stdin.encode() if isinstance(stdin, str) else stdin or b"")
        expiry = start + timeout
        try:
            with selectors.DefaultSelector() as poll:
                poll.register(child.stdout, selectors.EVENT_READ, out)
                poll.register(child.stderr, selectors.EVENT_READ, err)
                if child.stdin:
                    os.set_blocking(child.stdin.fileno(), False)
                    if incoming:
                        poll.register(child.stdin, selectors.EVENT_WRITE, None)
                    else:
                        child.stdin.close()
                while poll.get_map():
                    remaining = expiry - time.monotonic()
                    need(remaining > 0, f"CLI timeout: {label}")
                    ready = poll.select(remaining)
                    need(ready, f"CLI timeout: {label}")
                    for key, _ in ready:
                        if key.data is None:
                            try:
                                sent = os.write(key.fd, incoming)
                            except BrokenPipeError:
                                sent = len(incoming)
                            incoming = incoming[sent:]
                            if not incoming:
                                poll.unregister(key.fileobj)
                                key.fileobj.close()
                        else:
                            chunk = os.read(key.fd, 65536)
                            if not chunk:
                                poll.unregister(key.fileobj)
                            else:
                                key.data.extend(chunk)
                                need(len(out) + len(err) <= MAX_OUTPUT, f"CLI output exceeded bound: {label}")
            child.wait(timeout=max(0.1, expiry - time.monotonic()))
        finally:
            if child._smoke_process_group:
                # The stdio helper owns a native MCP child. Kill its private
                # process group even if the helper died before running cleanup.
                try:
                    os.killpg(child.pid, signal.SIGKILL)
                except ProcessLookupError:
                    pass
            if child.poll() is None:
                child.kill()
                child.wait(timeout=5)
            child.stdout.close()
            child.stderr.close()
            if child.stdin and not child.stdin.closed:
                child.stdin.close()
        self.receipt["commands"].append({"label": label, "argv_sha256": digest(encode(argv)),
            "exit": child.returncode, "expected_success": ok, "stdout_bytes": len(out), "stderr_bytes": len(err),
            "stdout_sha256": digest(out), "stderr_sha256": digest(err),
            "elapsed_ms": round((time.monotonic() - start) * 1000)})
        need(ok or child.returncode > 0, f"CLI refusal must not be a crash or signal: {label}")
        need((child.returncode == 0) == ok, f"unexpected CLI exit for {label}: {child.returncode}; stderr hash {digest(err)}")
        if not ok or mode == "text":
            return out.decode(), err.decode()
        if mode == "ndjson":
            return [json.loads(line) for line in out.splitlines() if line.strip()]
        return json.loads(out)

    def cli(self, scope, *args, label=None, stdin=None, ok=True, mode="json"):
        tool = {"reconcile": "contradictions" if len(args) == 1 else "reconcile",
                "snapshot": "snapshot_create"}.get(args[0], args[0])
        self.pace(tool)
        return self.command(label or f"{scope}.{'.'.join(args[:2])}",
            ["--json", *(["--user"] if scope == "user" else []), *args], stdin=stdin, ok=ok, mode=mode)

    def tool(self, client, name, args, *, label=None, ok=True, contains=None):
        self.pace(name, args)
        client.timeout = self.remaining()
        start = time.monotonic()
        try:
            value = client.call_tool(name, args)
        except McpError as error:
            self.receipt["tools"].append({"label": label or name, "tool": name,
                "arguments_sha256": digest(encode(args)), "expected_success": ok, "success": False,
                "error_kind": type(error).__name__, "elapsed_ms": round((time.monotonic() - start) * 1000)})
            need(not ok, f"MCP unexpectedly refused {label or name}: {error}")
            need(not isinstance(error, McpTransportError), f"transport failure is not a semantic refusal: {label or name}")
            if contains:
                need(contains in str(error), f"MCP refusal {label or name} lacked {contains!r}")
            return None
        self.receipt["tools"].append({"label": label or name, "tool": name,
            "arguments_sha256": digest(encode(args)), "expected_success": ok, "success": True,
            "result_sha256": digest(encode(value)), "result_bytes": len(encode(value)),
            "elapsed_ms": round((time.monotonic() - start) * 1000)})
        need(ok, f"MCP unexpectedly accepted {label or name}")
        return value

    def start_host(self, names, label, *, feedback=True):
        timeout = self.remaining()
        with socket.socket() as sock:
            sock.bind(("127.0.0.1", 0))
            port = sock.getsockname()[1]
        log = (self.root / f"{label}.log").open("wb")
        self.logs.append(log)
        argv = [str(self.options.mcp), "--capability-profile", "operator"]
        if feedback:
            argv += ["--allow-direct-feedback"]
        for name in names:
            argv += ["--db", f"{name}={self.db[name]}"]
        argv += ["--http", f"127.0.0.1:{port}"]
        child = subprocess.Popen(argv, cwd=self.root, env=self.env, stdin=subprocess.DEVNULL, stdout=log, stderr=log)
        self.children.append(child)
        end = time.monotonic() + timeout
        while time.monotonic() < end:
            need(child.poll() is None, f"native MCP {label} startup exit {child.returncode}; log hash {digest(Path(log.name).read_bytes())}")
            try:
                with socket.create_connection(("127.0.0.1", port), timeout=min(0.2, self.remaining())):
                    break
            except OSError:
                time.sleep(0.05)
        else:
            raise RuntimeError(f"native MCP {label} startup timeout")
        client = McpClient(f"http://127.0.0.1:{port}/", timeout=self.remaining())
        self.clients.append(client)
        client.connect()
        self.receipt["hosts"].append({"label": label, "url": client.url, "databases": names,
            "argv_sha256": digest(encode(argv)), "protocol": client.connect_result.get("protocolVersion"), "server": client.connect_result.get("serverInfo")})
        return child, client

    @staticmethod
    def stop_host(child, client):
        client.close()
        if child.poll() is None:
            child.terminate()
            try:
                child.wait(timeout=5)
            except subprocess.TimeoutExpired:
                child.kill()
                child.wait(timeout=5)

    def enroll(self, scope, url, **overrides):
        record = {"schema": "mneme.cli.owner.v1", "url": url, "database": scope, "db_id": self.ids[scope]}
        record.update(overrides)
        write_json(self.enrollment[scope], record)

    def claim(self, key, **extra):
        return {"source": {"namespace": "owner-smoke", "key": key, "reference": f"smoke://owner/{key}"},
                "summary": f"Violet lantern fixture {key}", "body": f"Disposable native evidence {key}", **extra}

    def bootstrap(self):
        for scope, db in self.db.items():
            init = self.command(f"offline.{scope}.capture-init", ["--json", "--db", str(db), "capture", "init"])
            need(init.get("status") == "initialized" and db.is_file(), "capture init did not provision current store")
            note = self.command(f"offline.{scope}.seed", ["--json", "--db", str(db), "save",
                f"{scope.upper()}_OWNER_SENTINEL", "--body", f"{scope.upper()}_BODY_SENTINEL", "--tags", "core,owner-smoke"])
            self.sentinels[scope] = note["id"]
            self.command(f"offline.{scope}.existing-init-refusal", ["--json", "--db", str(db), "capture", "init"], ok=False)
        before = tree_hash(self.blank)
        self.command("offline.bootstrap-inspect.absent", ["--json", "bootstrap-inspect", "--root", str(self.blank)], cwd=self.blank)
        need(tree_hash(self.blank) == before, "bootstrap-inspect created artifacts")
        for name, args in (("migrate", ["migrate"]), ("reembed", ["reembed"]),
                           ("upgrade", ["single-graph-upgrade", "--backend", "sqlite", "--output", str(self.blank / "successor.db")])):
            self.command(f"offline.{name}.absent-refusal", ["--json", "--db", str(self.blank / "absent.db"), *args], cwd=self.blank, ok=False)
            need(not (self.blank / "absent.db").exists() and not (self.blank / "successor.db").exists(),
                 f"offline {name} refusal created a database")

    def inventory(self, scope):
        result, ids, after = [], set(), None
        for index in range(64):
            args = ["list", "--limit", "7"] + (["--after", after] if after else [])
            page = self.cli(scope, *args, label=f"{scope}.list-page-{index}")
            need(page["db_id"] == self.ids[scope], "inventory wrong database identity")
            current = {item["id"] for item in page["items"]}
            need(not ids & current, "inventory repeated a node")
            ids |= current
            result.extend(page["items"])
            after = page.get("next_cursor")
            if after is None:
                return result
        raise RuntimeError("fixture node inventory exceeded 64 pages")

    def scope_matrix(self, scope, client):
        sentinel = self.sentinels[scope]
        other = self.sentinels["user" if scope == "project" else "project"]
        status = self.cli(scope, "status", label=f"{scope}.status-json")
        need(isinstance(status, dict), "status lacks object")
        # Human representation shares the same owner, but consumes the cold lane.
        self.pace("status")
        self.command(f"{scope}.status-human", [*(["--user"] if scope == "user" else []), "status"], mode="text")
        core = self.cli(scope, "core", label=f"{scope}.core")
        need(sentinel in {row["id"] for row in core["nodes"]} and other not in {row["id"] for row in core["nodes"]}, "core mixed project/global owners")
        note = self.cli(scope, "save", f"Violet lantern note {scope}", "--body", "Violet lantern note body", "--tags", "owner-smoke", label=f"{scope}.save-note")
        note_id = note["id"]
        made = self.cli(scope, "save", f"Violet lantern scene {scope}", "--kind", "episode", label=f"{scope}.save-episode")
        root, edition = made["episode_id"], made["edition_id"]
        captured = self.cli(scope, "capture", "add", "--input", "-", stdin=encode(self.claim(f"{scope}-capture")), label=f"{scope}.capture-add")
        replay = self.cli(scope, "capture", "add", "--input", "-", stdin=encode(self.claim(f"{scope}-capture")), label=f"{scope}.capture-replay")
        need(replay["id"] == captured["id"] and replay["replayed"] is True, "capture replay lost identity")
        appended = self.cli(scope, "episode", "append", "--input", "-", stdin=encode(self.claim(f"{scope}-append", thread="owner-smoke")), label=f"{scope}.episode-append")
        episode_page = self.cli(scope, "episode", "list", "--limit", "1", label=f"{scope}.episode-list")
        if episode_page.get("next"):
            self.cli(scope, "episode", "list", "--limit", "1", "--after", episode_page["next"], label=f"{scope}.episode-list-next")
        self.cli(scope, "episode", "search", "Violet lantern", "--limit", "2", label=f"{scope}.episode-search")
        before = self.cli(scope, "episode", "get", root, "--body", label=f"{scope}.episode-get")
        need(before["edition_id"] == edition, "episode latest differs")
        revised = self.cli(scope, "episode", "revise", root, "--input", "-", stdin=encode(self.claim(f"{scope}-revision",
            expected_edition_id=edition, reason="Correct disposable scene")), label=f"{scope}.episode-revise")
        need(revised["edition_id"] != edition, "episode revise failed to make immutable successor")
        self.cli(scope, "episode", "get", root, "--edition-id", edition, label=f"{scope}.episode-historical-get")
        history = self.cli(scope, "episode", "history", root, "--limit", "1", label=f"{scope}.episode-history")
        if history.get("next"):
            self.cli(scope, "episode", "history", root, "--limit", "1", "--after", history["next"], label=f"{scope}.episode-history-next")
        self.cli(scope, "episode", "references", appended["edition_id"], label=f"{scope}.episode-references")
        self.cli(scope, "query", "Violet lantern", "--k", "4", label=f"{scope}.query")
        context = self.cli(scope, "recall-context", "Violet lantern", label=f"{scope}.recall-context")
        need(context.get("schema") == "mneme.context.v7", "recall-context schema is not current")
        got = self.cli(scope, "get", note_id, "--body", "--edges", label=f"{scope}.get-body-edges")
        need(got["body"] == "Violet lantern note body", "GET did not read written body")
        chunk = self.cli(scope, "body", note_id, "--offset", "1", "--max-bytes", "6", label=f"{scope}.body-range")
        need(chunk["body"] == "iolet ", "body range differs")
        for destination in (sentinel, captured["id"]):
            self.cli(scope, "link", "--from", note_id, "--to", destination, "--weight", "0.8", label=f"{scope}.link-{destination}")
        page = self.cli(scope, "neighbors", note_id, "--limit", "1", label=f"{scope}.neighbors-page-0")
        need(page["items"] and page.get("next_cursor"), "neighbor fixture has no continuation")
        self.cli(scope, "neighbors", note_id, "--limit", "1", "--after", page["next_cursor"], label=f"{scope}.neighbors-page-1")
        self.cli(scope, "remote", note_id, label=f"{scope}.remote-edges")
        listed = self.inventory(scope)
        need({sentinel, note_id, captured["id"], edition, revised["edition_id"]} <= {row["id"] for row in listed}, "inventory omitted fixture nodes")
        filtered = self.cli(scope, "list", "--status", "active", "--tag", "owner-smoke", "--limit", "1", label=f"{scope}.list-filter")
        need(filtered["items"] and filtered.get("next_cursor"), "filtered inventory did not page")
        self.cli(scope, "list", "--status", "active", "--tag", "owner-smoke", "--limit", "1", "--after", filtered["next_cursor"], label=f"{scope}.list-filter-page-1")
        self.cli(scope, "list", "--status", "archived", label=f"{scope}.list-archived")
        self.cli(scope, "list", "--touchstones", label=f"{scope}.list-touchstones")
        self.cli(scope, "concern", "--input", "-", stdin=encode({"action": "list", "endpoint": note_id, "limit": 1}), label=f"{scope}.concern-list")
        raw = self.cli(scope, "ingest", "--summary", f"Violet lantern raw {scope}", "--body", "Raw disposable fixture", label=f"{scope}.ingest")
        self.cli(scope, "contradict", "--a", note_id, "--b", raw["id"], label=f"{scope}.contradict")
        self.cli(scope, "reconcile", label=f"{scope}.reconcile-list")
        self.cli(scope, "reconcile", "--a", note_id, "--b", raw["id"], "--as", "context-dependent", label=f"{scope}.reconcile-verdict")
        self.cli(scope, "feedback", "not-new", "--from", note_id, "--to", raw["id"], label=f"{scope}.feedback-not-new")
        self.cli(scope, "merges", label=f"{scope}.merges")
        self.cli(scope, "merge", "keep", "--a", note_id, "--b", raw["id"], label=f"{scope}.merge-keep")
        merge_loser = self.cli(scope, "save", "Violet lantern duplicate", label=f"{scope}.merge-loser-save")["id"]
        self.cli(scope, "feedback", "not-new", "--from", note_id, "--to", merge_loser, label=f"{scope}.merge-full-candidacy")
        self.cli(scope, "merge", "full", "--winner", note_id, "--loser", merge_loser, label=f"{scope}.merge-full")
        need(self.cli(scope, "get", merge_loser, label=f"{scope}.merge-archive-readback")["status"] == "archived", "merge did not archive loser")
        supersede_loser = self.cli(scope, "save", "Violet lantern outdated instruction", label=f"{scope}.supersede-loser-save")["id"]
        self.cli(scope, "contradict", "--a", note_id, "--b", supersede_loser, label=f"{scope}.supersede-contradiction")
        self.cli(scope, "supersede", "--winner", note_id, "--loser", supersede_loser, label=f"{scope}.supersede")
        self.cli(scope, "feedback", "relevant", "--to", note_id, label=f"{scope}.feedback-node")
        self.cli(scope, "decay", label=f"{scope}.decay")
        self.cli(scope, "prune", label=f"{scope}.prune")
        walk = self.cli(scope, "repl", note_id, "--budget", "4", stdin=f"look\nedges\nbody\ngo {sentinel}\nback\ndone {note_id}\n", mode="ndjson", label=f"{scope}.repl-reflect")
        need(walk[-1].get("reflected", {}).get("reinforced", 0) >= 1, "REPL done failed to reflect")
        aborted = self.cli(scope, "repl", note_id, stdin="look\nabort\n", mode="ndjson", label=f"{scope}.repl-abort")
        need(aborted[-1]["reflected"] is None, "REPL abort trained")
        forgotten = self.cli(scope, "save", "Disposable delete target", label=f"{scope}.forget-target")["id"]
        self.cli(scope, "forget", forgotten, label=f"{scope}.forget")
        self.cli(scope, "get", forgotten, ok=False, label=f"{scope}.forgotten-get-refusal")
        snapshot = self.cli(scope, "snapshot", "create", label=f"{scope}.snapshot-create")
        need(snapshot.get("db_id") == self.ids[scope], "snapshot receipt wrong owner")
        bundle = Path(snapshot["bundle"]).resolve()
        need(self.root in bundle.parents and (bundle / "database.db").is_file(), "snapshot bundle escaped fixture or lacks database")
        manifest_path = bundle / "manifest.json"
        manifest = json.loads(manifest_path.read_bytes())
        need(manifest["db_id"] == self.ids[scope], "snapshot manifest identity differs")
        self.receipt.setdefault("snapshots", []).append({"db": scope, "manifest": artifact(manifest_path), "files": len(manifest["files"])})
        self.cli(scope, "get", sentinel, label=f"{scope}.post-snapshot-reopen")
        # Offline admin is not an owner mutation and must not invent its success.
        for command in ("migrate", "reembed", "capture"):
            args = [command, "init"] if command == "capture" else [command]
            self.cli(scope, *args, ok=False, label=f"{scope}.offline-{command}-refusal")
        return note_id

    def run_tag_stewardship(self):
        """Focused native/host workflow; the classifier is scripted, never a provider."""
        import stewardship
        import tag_context
        from librarian_policy import LibrarianBudget
        self.prepare_cache()
        version, _ = self.command("artifact.cli-version", ["--version"], mode="text")
        self.receipt["cli_version"] = version.strip()
        self.bootstrap()
        child, client = self.start_host(["project"], "tag-stewardship", feedback=False)
        rows = self.tool(client, "databases", {})
        self.ids["project"] = rows[0]["db_id"]
        self.enroll("project", client.url)
        db_id = self.ids["project"]
        guide = self.tool(client, "save", {"db": "project", "summary":
            "Use people when a person is a substantial subject; incidental mentions do not qualify. "
            "Keep exact handles and independent topic tags. Tags do not establish current membership.",
            "body": "Background only: deliberately not classification policy.", "tags": ["tag-guide"]})["id"]
        person = self.tool(client, "save", {"db": "project", "summary":
            "Mara is a Rust compiler contributor whose work concerns type-system invariants.",
            "tags": ["rust", "mara-fixture"]})["id"]
        incidental = self.tool(client, "save", {"db": "project", "summary":
            "The API review mentioned Mara while comparing two iterator signatures.",
            "tags": ["api-design"]})["id"]
        args = {"db": "project", "expected_db_id": db_id}
        before = self.tool(client, "get", {**args, "id": person, "body": True})
        old_guide = self.tool(client, "get", {**args, "id": guide})
        self.tool(client, "edit_summary", {**args, "id": guide,
            "expected_snapshot_sha256": old_guide["summary_snapshot"]["expected_snapshot_sha256"],
            "summary": old_guide["summary"] + " Preserve rare but supported cross-domain connections."})
        self.tool(client, "retag", {**args, "id": person, "expected_tags": before["tags"],
            "tags": sorted([*before["tags"], "people"]),
            "expected_content_fingerprint": before["content_fingerprint"],
            "guard_nodes": [{"id": guide, "content_fingerprint": old_guide["content_fingerprint"]}]},
            ok=False, label="stewardship.stale-guide-refusal")
        need(self.tool(client, "get", {**args, "id": person, "body": True}) == before,
             "stale guide refusal changed canonical note")
        service = self.root / "stewardship-service.json"
        write_json(service, {"mode": "connect", "url": client.url, "database_name": "project",
                             "database_path": str(self.db["project"])})
        config = {"tag_stewardship": True, "tag_guide_id": guide, "recording_mode": "automatic",
                  "memory_mode": "async", "reader_model": "gpt-6.1-sol", "librarian_effort": "low",
                  "service_config": service, "project_root": self.project}
        # Explicitly test real wire admission without the fake-owner unit seam.
        for effort in ("low", "medium"):
            config["librarian_effort"] = effort
            budget = LibrarianBudget(effort=effort)
            with stewardship.NativeOwner(config, budget) as owner:
                owner.authorize()
                target, _ = owner.target(person)
                need(target is not None and target["id"] == person, "native target rejected a semantic note")
                context = tag_context.collect(config, expected_db_id=db_id, timeout=self.remaining(),
                    max_bytes=budget.recording_hint_bytes, cue=target["summary"], seed_tags=target["tags"])
                need(context.enabled and context.guide["id"] == guide,
                     f"{effort} real guide/vocabulary context was unavailable")
                need(context.decoded_bytes <= budget.recording_hint_bytes, "tag lookup exceeded aggregate envelope")
        config["librarian_effort"] = "medium"
        class Classifier:
            calls = 0
            def steward(self, targets, context, **_):
                self.calls += 1
                return {"provider_attempt": True, "usage": {"input_tokens": 100, "output_tokens": 20},
                        "decisions": [{"id": node["id"],
                            "disposition": "retag" if node["id"] == person and "people" not in node["tags"] else "noop",
                            "tags": sorted([*node["tags"], "people"]) if node["id"] == person and "people" not in node["tags"] else node["tags"]}
                            for node in targets]}
        model = Classifier()
        reservations = set()
        accounted = []
        def reserve(key):
            need(key not in reservations, "duplicate session reservation")
            reservations.add(key)
            return True
        def account(key, result):
            need(key in reservations, "usage settled without reservation")
            reservations.remove(key)
            accounted.append(result["usage"])
            return True
        need(stewardship.enqueue(config, db_id, [person, incidental]), "changed-node enqueue failed")
        need(stewardship.step(config, "native-smoke", model, reserve, account),
             "real NativeOwner stewardship step made no progress")
        after = self.tool(client, "get", {**args, "id": person, "body": True})
        need(set(after["tags"]) == {*before["tags"], "people"}, "native guarded retag did not apply")
        for key in ("id", "summary", "body", "body_revision", "provenance", "created", "exposure_count", "grounded_use_count"):
            need(after[key] == before[key], f"retag changed unrelated {key}")
        got_incidental = self.tool(client, "get", {**args, "id": incidental})
        need(got_incidental["tags"] == ["api-design"], "scripted incidental fixture changed unexpectedly")
        vocab = self.cli("project", "list", "--tags", "--prefix", "pe", label="stewardship.vocabulary-cli")
        need([row["name"] for row in vocab["items"]] == ["people"], "tag prefix list differs after retag")
        native_vocab = self.tool(client, "list", {**args, "kind": "tags", "prefix": "pe"})
        need(native_vocab["items"] == vocab["items"], "CLI/MCP vocabulary differs")
        status = stewardship.inspect(db_id)
        need(status["outcome"] == "available" and any(a["status"] == "applied" for a in status["actions"]),
             "journal did not retain verified edit acknowledgement")
        # A new process/session may revisit changed tags once; then same content
        # is stable, rather than repeatedly spending a model call on a no-op.
        for _ in range(2):
            with stewardship.Journal(db_id) as journal:
                journal.put("next_batch", 0)
                journal.enqueue([person, incidental])
            stewardship.step(config, "reopened-native-smoke", model, reserve, account)
        need(model.calls == 2 and not reservations and len(accounted) == 2,
             "unchanged examined content churned or accounting was lost")
        self.receipt["checks"].extend(["real_native_owner_target_and_low_medium_context",
            "stale_guide_atomic_refusal", "native_guarded_retag_preserves_content_and_learning",
            "cli_mcp_tag_vocabulary_parity", "owner_journal_reopen_and_noop_stability"])
        self.receipt["stewardship"] = {"provider_calls": 0, "scripted_classifications": model.calls,
            "session_accounting": "injected reservation callbacks; separate worker component tests cover parent ledger",
            "native_owner": "real copied CLI bridge and HTTP server", "journal": "disposable device-local SQLite"}
        self.stop_host(child, client)
        self.command("stewardship.offline-reopen", ["--json", "--db", str(self.db["project"]), "get", person])
        if self.options.stdio:
            for scope, cue in (("project", "Mneme tagged retrieval and F7 migration"),
                               ("user", "user preferences and working style")):
                self.command(f"offline.{scope}.stdio-cue-save", ["--json", "--db", str(self.db[scope]), "save", cue])
            helper = Path(__file__).with_name("mcp_stdio_smoke.py").resolve()
            self.receipt["artifacts"]["stdio_harness"] = artifact(helper)
            result = self.command("artifact.stdio-smoke", [str(helper), "--binary", str(self.options.mcp),
                "--user-db", str(self.db["user"]), "--project-db", str(self.db["project"]),
                "--capability-profile", "operator", "--timeout", str(self.remaining())], executable=Path(sys.executable))
            need("list" in result["advertised_tools"] and result["exit_code"] == 0,
                 "stdio smoke lacks vocabulary operation or clean exit")
            self.receipt["stdio"] = result
            self.receipt["omissions"] = [item for item in self.receipt["omissions"] if item["surface"] != "MCP stdio"]
            self.receipt["checks"].append("native_stdio_catalog_read_refusal_shutdown")
            for scope, db in self.db.items():
                self.command(f"offline.{scope}.reopen-after-stdio", ["--json", "--db", str(db), "get", self.sentinels[scope]])

    def run(self):
        self.prepare_cache()
        version, _ = self.command("artifact.cli-version", ["--version"], mode="text")
        self.receipt["cli_version"] = version.strip()
        self.bootstrap()
        child, client = self.start_host(["project", "user"], "two-store")
        catalog = client.list_tools()
        names = {row["name"] for row in catalog}
        need({"graph", "list", "neighbors", "save", "walk", "reflect", "database_control"} <= names, "owner catalog lacks required surface")
        self.receipt["catalog"] = {"names": sorted(names), "sha256": digest(encode(catalog))}
        rows = self.tool(client, "databases", {})
        self.ids = {row["db"]: row["db_id"] for row in rows}
        need(set(self.ids) == {"project", "user"} and len(set(self.ids.values())) == 2, "two-store fixture identity mismatch")
        self.receipt["database_ids"] = self.ids
        for scope in self.db:
            self.enroll(scope, client.url)
        note_ids = {scope: self.scope_matrix(scope, client) for scope in self.db}
        self.cli("user", "link", "--from", note_ids["user"], "--to", note_ids["project"], "--to-remote-db", "project", label="user.cross-owner-link")
        remote = self.cli("user", "remote", note_ids["user"], "--limit", "1", label="user.cross-owner-remote-page")
        need(remote.get("items"), "cross-owner remote page omitted link")
        bulk_ids = []
        sparse_ids = set()
        for index in range(70):
            tags = ["sparse-smoke"] if index in (0, 69) else ["bulk-smoke"]
            row = self.tool(client, "save", {"db": "project", "kind": "note", "summary": f"Finite inventory fixture {index}",
                "tags": tags, "operation_id": f"bulk-smoke-{index}"}, label=f"native.project.bulk-save-{index}")
            bulk_ids.append(row["id"])
            if "sparse-smoke" in tags:
                sparse_ids.add(row["id"])
        listed = self.inventory("project")
        need(len(listed) > 64 and set(bulk_ids) <= {row["id"] for row in listed}, "inventory lost nodes across storage batch boundary")
        found, after = set(), None
        for index in range(64):
            args = ["list", "--tag", "sparse-smoke", "--limit", "1"] + (["--after", after] if after else [])
            page = self.cli("project", *args, label=f"project.sparse-filter-page-{index}")
            found.update(row["id"] for row in page["items"])
            after = page.get("next_cursor")
            if after is None:
                break
        else:
            raise RuntimeError("sparse fixture inventory exceeded 64 pages")
        need(found == sparse_ids, "sparse filtered pages omitted or added a match")
        self.receipt["checks"].append("node_inventory_over_64_and_sparse_filters")
        self.native_matrix(client, note_ids)
        self.no_fallback(client)
        self.stop_host(child, client)
        for scope in ("user", "project"):
            child, client = self.start_host([scope], f"sole-{scope}", feedback=False)
            got = self.tool(client, "get", {"id": self.sentinels[scope], "expected_db_id": self.ids[scope]}, label=f"sole-{scope}.omitted-get")
            need(got["db"] == scope, "sole-owner omitted scope failed")
            self.tool(client, "list", {"limit": 1}, label=f"sole-{scope}.omitted-list")
            self.tool(client, "status", {}, label=f"sole-{scope}.omitted-status")
            self.tool(client, "feedback", {"db": scope, "to": note_ids[scope], "signal": "relevant"},
                      ok=False, label=f"sole-{scope}.direct-feedback-opt-in-refusal")
            self.stop_host(child, client)
        # Reopen after all owner shutdowns proves the final lease was released.
        for scope, db in self.db.items():
            self.command(f"offline.{scope}.reopen-after-shutdown", ["--json", "--db", str(db), "get", self.sentinels[scope]])
        self.receipt["checks"].append("final_owner_shutdown_and_offline_reopen")
        if getattr(self.options, "stdio", False):
            # Exact cue seeds make the existing finite stdio smoke's primary
            # assertions meaningful under both hashing and BGE-base builds.
            for scope, cue in (("project", "Mneme tagged retrieval and F7 migration"),
                               ("user", "user preferences and working style")):
                self.command(f"offline.{scope}.stdio-cue-save", ["--json", "--db", str(self.db[scope]), "save", cue])
            helper = Path(__file__).with_name("mcp_stdio_smoke.py").resolve()
            self.receipt["artifacts"]["stdio_harness"] = artifact(helper)
            result = self.command("artifact.stdio-smoke", [str(helper), "--binary", str(self.options.mcp),
                "--user-db", str(self.db["user"]), "--project-db", str(self.db["project"]),
                "--capability-profile", "operator", "--timeout", str(self.remaining())], executable=Path(sys.executable))
            need("graph" in result["advertised_tools"] and result["exit_code"] == 0, "stdio smoke lacks current graph catalog or clean exit")
            self.receipt["stdio"] = result
            self.receipt["omissions"] = [item for item in self.receipt["omissions"] if item["surface"] != "MCP stdio"]
            self.receipt["checks"].append("native_stdio_catalog_context_denial_shutdown")
            for scope, db in self.db.items():
                self.command(f"offline.{scope}.reopen-after-stdio", ["--json", "--db", str(db), "get", self.sentinels[scope]])

    def native_matrix(self, client, notes):
        for tool, args in (("get", {"id": self.sentinels["project"]}), ("list", {"limit": 1}),
                           ("episode", {"action": "list", "limit": 1}), ("neighbors", {"id": notes["project"], "limit": 1}),
                           ("status", {}), ("core", {}), ("recall_context", {"text": "Violet lantern"})):
            value = self.tool(client, tool, {"expected_db_id": self.ids["project"], **args}, label=f"native.omitted-project.{tool}")
            if isinstance(value, dict) and "db" in value:
                need(value["db"] == "project", f"{tool} omitted scope did not select project")
        for scope in self.db:
            # Walk the entire lightweight topology independently of summaries.
            seen_nodes, seen_edges, after = set(), set(), None
            for index in range(64):
                args = {"db": scope, "action": "topology", "limit": 16}
                if after:
                    args["after"] = after
                topology = self.tool(client, "graph", args, label=f"native.{scope}.graph-topology-page-{index}")
                need(topology["db_id"] == self.ids[scope], "graph page identity differs")
                nodes = {row["id"] for row in topology["nodes"]}
                edges = {(row["from"], row["to"]) for row in topology["edges"]}
                need(not nodes & seen_nodes and not edges & seen_edges, "topology repeated a record")
                need(all("summary" not in row and "body" not in row for row in topology["nodes"]), "topology hydrated cards")
                seen_nodes |= nodes
                seen_edges |= edges
                after = topology.get("next_cursor")
                if after is None:
                    need(topology["coverage"]["complete"], "graph exhausted without complete coverage")
                    break
            else:
                raise RuntimeError("fixture topology exceeded 64 pages")
            need({self.sentinels[scope], notes[scope]} <= seen_nodes and seen_edges, "whole graph omitted nodes or edges")
            summaries = self.tool(client, "graph", {"db": scope, "action": "summaries", "ids": [self.sentinels[scope], notes[scope]]}, label=f"native.{scope}.graph-summaries-batch")
            need({row["id"] for row in summaries["items"]} == {self.sentinels[scope], notes[scope]}, "summary batch identities differ")
            need(all(not row["missing"] and row.get("summary") and "body" not in row for row in summaries["items"]), "summary batch missing or leaked bodies")
        # Guard checks happen before effects; readbacks before/after are compared,
        # including exposure metadata. No malformed guard may create a node.
        before = self.tool(client, "get", {"db": "project", "id": notes["project"], "body": True})
        inventory_before = self.inventory("project")
        guarded = (("get", {"id": notes["project"]}), ("list", {"limit": 1}),
                   ("graph", {"action": "topology", "limit": 1}), ("neighbors", {"id": notes["project"]}),
                   ("link", {"from": notes["project"], "to": self.sentinels["project"], "weight": 0.1}),
                   ("supersede", {"winner": notes["project"], "loser": self.sentinels["project"]}),
                   ("contradict", {"a": notes["project"], "b": self.sentinels["project"]}),
                   ("reconcile", {"a": notes["project"], "b": self.sentinels["project"], "resolution": "unresolved"}),
                   ("merge", {"mode": "full", "winner": notes["project"], "loser": self.sentinels["project"]}),
                   ("decay", {}), ("prune", {}),
                   ("episode", {"action": "append", **self.claim("bad-guard-episode")}),
                   ("capture", self.claim("bad-guard-capture")),
                   ("save", {"summary": "Must not save"}), ("ingest", {"summary": "Must not ingest"}),
                   ("forget", {"id": notes["project"]}), ("feedback", {"to": notes["project"], "signal": "relevant"}),
                   ("database_control", {"action": "release"}), ("snapshot_create", {}))
        for tool, args in guarded:
            self.tool(client, tool, {"db": "project", "expected_db_id": self.ids["user"], **args},
                      ok=False, contains="expected_db_id mismatch", label=f"native.guard-mismatch.{tool}")
        after = self.tool(client, "get", {"db": "project", "id": notes["project"], "body": True})
        need(before == after, "identity mismatch changed node state")
        need(inventory_before == self.inventory("project"), "identity mismatch changed graph inventory")
        self.tool(client, "save", {"db": "project", "expected_db_id": "invalid", "summary": "Must not save"},
                  ok=False, label="native.malformed-guard-refusal")
        self.tool(client, "status", {"db": "does-not-exist"}, ok=False, label="native.unknown-db-refusal")
        for scope in self.db:
            status = self.tool(client, "database_control", {"db": scope, "action": "status"})
            need(status["state"] == "open", "database status not open")
            released = self.tool(client, "database_control", {"db": scope, "action": "release", "expected_db_id": self.ids[scope]})
            need(released["state"] == "maintenance", "database release failed")
            if scope == "project":
                self.tool(client, "get", {"id": self.sentinels["project"]}, ok=False, label="native.released-project-no-user-fallback")
            self.cli(scope, "get", self.sentinels[scope], ok=False, label=f"{scope}.released-owner-refusal")
            self.command(f"offline.{scope}.released-owner-reopen", ["--json", "--db", str(self.db[scope]), "get", self.sentinels[scope]])
            resumed = self.tool(client, "database_control", {"db": scope, "action": "resume", "expected_db_id": self.ids[scope]})
            need(resumed["state"] == "open", "database resume failed")
            self.cli(scope, "get", self.sentinels[scope], label=f"{scope}.resumed-read")
        self.receipt["checks"].extend(["project_global_command_matrix", "native_omitted_project_selection",
            "guard_mismatch_no_node_effects", "release_resume_offline_handoff", "graph_paging_and_batched_summaries"])

    def no_fallback(self, client):
        conventional = self.project / ".mneme/memory.db"
        need(not conventional.exists(), "unexpected conventional local store")
        original = self.enrollment["project"].read_bytes()
        self.enroll("project", client.url, database="does-not-exist")
        self.cli("project", "status", ok=False, label="project.bad-enrolled-target")
        self.enroll("project", client.url, db_id=self.ids["user"])
        self.cli("project", "save", "Must not write on mismatched owner", ok=False, label="project.bad-enrolled-identity")
        self.enroll("project", "http://127.0.0.1:9/")
        self.cli("project", "get", self.sentinels["project"], ok=False, label="project.unavailable-owner-no-fallback")
        self.enrollment["project"].write_text("{broken")
        self.cli("project", "get", self.sentinels["project"], ok=False, label="project.malformed-enrollment")
        self.cli("user", "get", self.sentinels["user"], label="global.ignores-malformed-project")
        self.enrollment["project"].write_bytes(original)
        original_global = self.enrollment["user"].read_bytes()
        self.enrollment["user"].unlink()
        self.cli("user", "status", ok=False, label="global.missing-owner-no-project-fallback")
        self.enrollment["user"].write_bytes(original_global)
        self.command("project.unenrolled-no-global-fallback", ["--json", "status"], cwd=self.blank, ok=False)
        # Explicit remote overrides enrollment but never retries another name.
        self.pace("status")
        self.command("explicit.remote-bad-target", ["--json", "--remote", client.url, "--remote-db", "missing", "status"], ok=False)
        need(not conventional.exists() and not (self.root / "data/mneme/memory.db").exists(), "refusal fell back to local store")
        self.receipt["checks"].append("bad_target_identity_malformed_missing_enrollment_no_fallback")

    def close(self):
        for client in reversed(self.clients):
            try:
                client.close()
            except Exception:
                pass
        for child in reversed(self.children):
            if child.poll() is None:
                child.terminate()
                try:
                    child.wait(timeout=3)
                except subprocess.TimeoutExpired:
                    child.kill()
                    child.wait(timeout=5)
        for log in self.logs:
            log.close()
        need(all(child.poll() is not None for child in self.children), "smoke left a child alive")


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--cli", required=True, type=Path)
    parser.add_argument("--mcp", required=True, type=Path)
    parser.add_argument("--tag-stewardship", action="store_true", help="run the focused real native-owner/guarded-retag workflow with a scripted classifier; no provider calls")
    parser.add_argument("--integration-root", type=Path, help="explicit copied integration lib directory; defaults to source for ordinary smoke")
    parser.add_argument("--stdio", action="store_true", help="also run the finite existing native stdio smoke after HTTP shutdown on these same disposable two stores")
    parser.add_argument("--embedding-cache", type=Path, help="optional populated BGE-base cache; only required assets are copied into the disposable fixture, no downloads")
    parser.add_argument("--build-description", default="not supplied; binary hashes are authoritative", help="caller-supplied build/features provenance, not inferred")
    parser.add_argument("--timeout", type=float, default=300, help="total operation deadline in seconds (max 900)")
    parser.add_argument("--output", type=Path, help="optional concise JSON receipt; stdout also receives receipt")
    options = parser.parse_args()
    if options.integration_root:
        import importlib
        root = options.integration_root.resolve(strict=True)
        need(root.is_dir() and (root / "mcp_client.py").is_file(), "invalid copied integration root")
        sys.path.insert(0, str(root))
        sys.modules.pop("mcp_client", None)
        module = importlib.import_module("mcp_client")
        need(Path(module.__file__).resolve().parent == root, "copied integration import selected wrong root")
        globals().update({name: getattr(module, name) for name in ("McpClient", "McpError", "McpTransportError")})
    need(30 <= options.timeout <= 900, "--timeout must be 30..900 seconds")
    options.cli, options.mcp = options.cli.resolve(), options.mcp.resolve()
    if options.embedding_cache:
        options.embedding_cache = options.embedding_cache.resolve()
    for binary in (options.cli, options.mcp):
        need(binary.is_file() and os.access(binary, os.X_OK), f"not an executable artifact: {binary}")
    if options.output:
        options.output = options.output.resolve()
        need(options.output not in (options.cli, options.mcp, Path(__file__).resolve()), "receipt must not overwrite an artifact")
        need(not options.output.exists(), "receipt destination already exists; choose a fresh path")
    receipt = {"schema": "mneme.cli-owner-artifact-smoke.v1", "status": "running",
        "scope": "disposable CLI owner + native MCP HTTP, not live enrollment or workload-quality validation",
        "build_description": options.build_description,
        "artifacts": {"cli": artifact(options.cli), "mcp": artifact(options.mcp), "harness": artifact(Path(__file__).resolve())},
        "commands": [], "tools": [], "hosts": [], "checks": [],
        "omissions": [{"surface": "demo", "reason": "independent ephemeral walkthrough; not a configured-owner operation"},
                      {"surface": "tui", "reason": "interactive terminal rendering; component/TUI tests, not piped CLI smoke"},
                      {"surface": "library", "reason": "independent read-only library authority; tools/memory_library_smoke.py covers it"},
                      {"surface": "bootstrap-create", "reason": "reviewed greenfield approval workflow, not ordinary owner command"},
                      {"surface": "migrate/reembed/single-graph-upgrade publication", "reason": "structural absent-target refusals only; dedicated migration/rebuild suites validate publication"},
                      {"surface": "MCP stdio", "reason": "HTTP/native bridge tested here; tools/mcp_stdio_smoke.py covers stdio"}]}
    if options.integration_root:
        receipt["integration_artifacts"] = [artifact(path) for path in sorted(root.glob("*.py"))]
    start = time.monotonic()
    try:
        with tempfile.TemporaryDirectory(prefix="mneme-cli-owner-smoke-") as directory:
            smoke = Smoke(options, Path(directory), receipt)
            try:
                smoke.run_tag_stewardship() if options.tag_stewardship else smoke.run()
                receipt["status"] = "passed"
            finally:
                smoke.close()
                receipt["cleanup"] = "all spawned native owner/client/CLI children exited; disposable tree removed on scope exit"
        for name in ("cli", "mcp"):
            need(artifact(getattr(options, name)) == receipt["artifacts"][name], f"{name} artifact changed during smoke")
        for entry in receipt.get("integration_artifacts", []):
            need(artifact(Path(entry["path"])) == entry, "copied integration changed during smoke")
    except Exception as error:
        receipt["status"] = "failed"
        receipt["error"] = str(error)
    receipt["elapsed_ms"] = round((time.monotonic() - start) * 1000)
    if options.output:
        options.output.parent.mkdir(parents=True, exist_ok=True)
        with options.output.open("xb") as output:
            output.write(encode(receipt) + b"\n")
    print(json.dumps(receipt, sort_keys=True, indent=2))
    return 0 if receipt["status"] == "passed" else 1


if __name__ == "__main__":
    sys.exit(main())
