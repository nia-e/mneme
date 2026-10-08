#!/usr/bin/env python3
"""Disposable native owner/snapshot/replica/library smoke. Never touches a live DB.

Pass built binaries explicitly; this script does not install or build anything.
It writes a compact receipt to --output and removes every fixture process/store.
"""

from __future__ import annotations

import argparse
import hashlib
import json
import os
from pathlib import Path
import selectors
import shutil
import socket
import subprocess
import sys
import tempfile
import time

sys.dont_write_bytecode = True
sys.path.insert(0, str(Path(__file__).resolve().parents[1] / "integrations" / "codex"))
from mcp_client import McpClient, McpProtocolError, PROTOCOL_VERSION  # noqa: E402


def need(condition: bool, message: str) -> None:
    if not condition:
        raise RuntimeError(message)


def free_port() -> int:
    with socket.socket() as sock:
        sock.bind(("127.0.0.1", 0))
        return sock.getsockname()[1]


class Host:
    def __init__(self, binary: Path, root: Path, name: str, args: list[str], env: dict[str, str]):
        self.port = free_port()
        self.url = f"http://127.0.0.1:{self.port}/"
        self.log = (root / f"{name}.log").open("wb")
        self.process = subprocess.Popen(
            [str(binary), *args, "--http", f"127.0.0.1:{self.port}"],
            cwd=root, env=env, stdin=subprocess.DEVNULL, stdout=self.log, stderr=self.log,
        )
        try:
            deadline = time.monotonic() + 30
            while time.monotonic() < deadline:
                need(self.process.poll() is None, f"{name} exited during startup; see {self.log.name}")
                try:
                    with socket.create_connection(("127.0.0.1", self.port), timeout=0.5):
                        break
                except OSError:
                    time.sleep(0.1)
            else:
                raise RuntimeError(f"{name} did not listen within 30 seconds")
        except Exception:
            self.stop()
            raise

    def stop(self) -> None:
        if self.process.poll() is None:
            self.process.terminate()
            try:
                self.process.wait(timeout=20)
            except subprocess.TimeoutExpired:
                self.process.kill()
                self.process.wait(timeout=5)
        self.log.close()


class StdioLibrary:
    def __init__(self, binary: Path, config: Path, root: Path, env: dict[str, str]):
        self.log = (root / "library-stdio.log").open("wb")
        self.process = subprocess.Popen(
            [str(binary), "--library-config", str(config)], cwd=root, env=env,
            stdin=subprocess.PIPE, stdout=subprocess.PIPE, stderr=self.log,
        )
        self.next_id = 1
        self.buffer = b""
        try:
            self.request("initialize", {"protocolVersion": PROTOCOL_VERSION,
                        "capabilities": {}, "clientInfo": {"name": "library-smoke", "version": "1"}})
            self.request("notifications/initialized", {}, notification=True)
        except Exception:
            self.stop()
            raise

    def request(self, method: str, params: dict, *, notification: bool = False):
        msg = {"jsonrpc": "2.0", "method": method, "params": params}
        if not notification:
            msg["id"] = self.next_id
            self.next_id += 1
        encoded = (json.dumps(msg, separators=(",", ":")) + "\n").encode()
        need(len(encoded) <= 128 * 1024, "stdio request over local bound")
        self.process.stdin.write(encoded)
        self.process.stdin.flush()
        if notification:
            return None
        deadline = time.monotonic() + 30
        with selectors.DefaultSelector() as selector:
            selector.register(self.process.stdout, selectors.EVENT_READ)
            while b"\n" not in self.buffer:
                remaining = deadline - time.monotonic()
                need(remaining > 0 and bool(selector.select(remaining)), f"stdio {method} timed out")
                chunk = os.read(self.process.stdout.fileno(), 65536)
                need(chunk, f"stdio closed before {method} response")
                self.buffer += chunk
                need(len(self.buffer) <= 512 * 1024, "oversized stdio frame")
        line, self.buffer = self.buffer.split(b"\n", 1)
        value = json.loads(line)
        need(value.get("id") == msg["id"] and "error" not in value, "stdio RPC refusal")
        return value["result"]

    def tool(self, name: str, args: dict):
        result = self.request("tools/call", {"name": name, "arguments": args})
        need(result.get("isError") is False, f"stdio {name} tool refusal")
        return json.loads(result["content"][0]["text"])

    def stop(self):
        if self.process.poll() is None:
            self.process.stdin.close()
            try:
                self.process.wait(timeout=10)
            except subprocess.TimeoutExpired:
                self.process.kill()
                self.process.wait(timeout=5)
        self.process.stdout.close()
        self.log.close()


def cli(binary: Path, root: Path, env: dict[str, str], args: list[str], *, ok=True):
    result = subprocess.run([str(binary), *args], cwd=root, env=env, capture_output=True,
                            text=True, timeout=30)
    need((result.returncode == 0) is ok, f"CLI {args!r}: {result.stderr[-600:]}")
    return json.loads(result.stdout) if ok else result.stderr


def write_json(path: Path, value) -> None:
    path.write_text(json.dumps(value, sort_keys=True, indent=2) + "\n", encoding="utf-8")


def artifact(path: Path) -> dict:
    digest = hashlib.sha256()
    with path.open("rb") as stream:
        for block in iter(lambda: stream.read(1024 * 1024), b""):
            digest.update(block)
    return {"path": str(path), "bytes": path.stat().st_size, "sha256": digest.hexdigest()}


def one_db(client: McpClient, name: str) -> str:
    rows = client.call_tool("databases", {})
    need(isinstance(rows, list) and len(rows) == 1 and rows[0]["db"] == name,
         f"unexpected {name} database catalog")
    return rows[0]["db_id"]


def episode_input(key: str, summary: str, body: str, **extra) -> dict:
    return {"source": {"namespace": "library-smoke", "key": key,
                       "reference": f"smoke://library/{key}"},
            "summary": summary, "body": body, "thread": "library-smoke",
            "occurred": {"kind": "point", "at": 1_790_000_000_000}, **extra}


def check_context(value: dict, episodes: dict[str, str], *, source_kind: str,
                  generation: str | None = None) -> None:
    need(value.get("schema") == "mneme.library.context.v3", "library did not emit context.v3")
    need("probationary" not in value, "library context retained an obsolete probation lane")
    need(len(json.dumps(value, ensure_ascii=False, separators=(",", ":")).encode()) <= 32768,
         "library mixed context exceeded its shared 32 KiB budget")
    semantic = value["primary"]
    need(len(semantic) + len(value["episodes"]) <= 13 and len(value["episodes"]) <= 4,
         "library episodic cards escaped global item limits")
    need(all(card.get("kind") == "semantic" for card in semantic),
         "library episode appeared in a semantic lane")
    for card in value["episodes"]:
        need(card["kind"] == "episode" and card["lane"] == "episodic"
             and card["id"] == card["edition_id"] == card["current_edition_id"],
             "library episode is not a typed current edition")
    for project, edition in episodes.items():
        cards = [card for card in value["episodes"] if card["project_id"] == project]
        need(any(card["id"] == edition for card in cards), f"missing current {project} episode {edition}")
        coverage = next(item for item in value["coverage"] if item["project_id"] == project)
        need(coverage["episodic"]["state"] == "searched", "selected source episodic coverage absent")
        pinned = [card for card in semantic + cards if card["project_id"] == project]
        need(all(card["source"] == coverage["source"] for card in pinned),
             "semantic and episodic lanes did not share the selected source")
        need(all(card["source"]["kind"] == source_kind
                 and card["source"]["generation"] == generation for card in pinned),
             "mixed library context silently mixed live/snapshot generations")


def main() -> None:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--mnemed", type=Path, required=True)
    parser.add_argument("--mcp", type=Path, required=True)
    parser.add_argument("--output", type=Path, required=True)
    parser.add_argument("--old-mcp", type=Path,
                        help="optional prior context.v3 owner for explicit incompatible-source refusal")
    parser.add_argument("--rerank", action="store_true",
                        help="also exercise real common-model ranking across both owners")
    options = parser.parse_args()
    for binary in (options.mnemed, options.mcp, options.old_mcp):
        if binary is None:
            continue
        need(binary.is_file() and os.access(binary, os.X_OK), f"binary missing: {binary}")
    mnemed, mcp = options.mnemed.resolve(), options.mcp.resolve()
    os.environ["MNEME_CLIENT_BINARY"] = str(mnemed)
    output = options.output.resolve()
    need(output not in (mnemed, mcp, options.old_mcp.resolve() if options.old_mcp else None),
         "receipt path must not overwrite a binary")
    output.parent.mkdir(parents=True, exist_ok=True)
    hosts: list[Host] = []
    stdio = None
    receipt = {"schema": "mneme.library-installed-smoke.v2", "status": "running",
               "harness": artifact(Path(__file__).resolve()),
               "artifacts": {"mnemed": artifact(mnemed), "mneme-mcp": artifact(mcp)},
               "rerank": options.rerank, "checks": [], "durations_ms": {}}
    if options.old_mcp:
        receipt["artifacts"]["old-mcp"] = artifact(options.old_mcp.resolve())
    with tempfile.TemporaryDirectory(prefix="mneme-library-smoke-") as dirname:
        root = Path(dirname)
        env = os.environ.copy()
        env.pop("MNEME_DB", None)
        env["MNEME_RERANK"] = "0"
        env["GIT_CEILING_DIRECTORIES"] = str(root)
        env.update({"HF_HUB_OFFLINE": "1", "TRANSFORMERS_OFFLINE": "1",
                    "HTTP_PROXY": "http://127.0.0.1:9", "HTTPS_PROXY": "http://127.0.0.1:9",
                    "ALL_PROXY": "http://127.0.0.1:9", "NO_PROXY": "127.0.0.1,::1,localhost"})
        for name in ("http_proxy", "https_proxy", "all_proxy", "no_proxy"):
            env[name] = env[name.upper()]
        os.environ.pop("MNEME_DB", None)
        os.environ.update(env)
        try:
            initial = {}
            for name in ("a", "b"):
                payload = root / f"episode-{name}.json"
                write_json(payload, episode_input(f"{name}-original", f"Library smoke {name} original scene",
                                                  f"EPISODE_{name}_ORIGINAL_BODY"))
                initial[name] = cli(mnemed, root, env, ["--json", "--db", str(root / f"owner-{name}.db"),
                                    "episode", "append", "--input", str(payload)])
            owner_a = Host(mcp, root, "owner-a", ["--capability-profile", "operator",
                           "--db", f"project={root / 'owner-a.db'}"], env)
            hosts.append(owner_a)
            owner_b = Host(mcp, root, "owner-b", ["--capability-profile", "operator",
                           "--db", f"project={root / 'owner-b.db'}"], env)
            hosts.append(owner_b)
            with McpClient(owner_a.url, timeout=30) as a, McpClient(owner_b.url, timeout=30) as b:
                db_a, db_b = one_db(a, "project"), one_db(b, "project")
                old = a.call_tool("ingest", {"db": "project", "summary": "ALPHA_OLD_SENTINEL library smoke", "body": "ALPHA_OLD_BODY managed smoke evidence"})
                other = b.call_tool("ingest", {"db": "project", "summary": "BETA_SENTINEL library smoke", "body": "BETA_BODY managed smoke evidence"})
                if options.rerank:
                    misleading = a.call_tool("ingest", {"db": "project", "summary":
                        "Harbor library cart field verification has NOT been completed; materials availability alone is not an operational check.",
                        "body": "Smoke fixture: no completed field verification in this owner."})
                    relevant = b.call_tool("ingest", {"db": "project", "summary":
                        "Harbor library cart field verification was completed; every item passed the operational checks.",
                        "body": "Smoke fixture: this owner confirms the completed field verification."})
                    need(misleading.get("id") and relevant.get("id"), "rank fixture ingest failed")
                need(old.get("id") and other.get("id"), "owner ingest did not return IDs")
                snapshot_episode = a.call_tool("episode", {"db": "project", "action": "revise",
                    "episode_id": initial["a"]["episode_id"],
                    **episode_input("a-snapshot", "Library smoke alpha corrected before snapshot", "ALPHA_EPISODE_SNAPSHOT_BODY",
                        expected_edition_id=initial["a"]["edition_id"], reason="Correct the original scene.")})
                snapshot = cli(mnemed, root, env, ["--remote", owner_a.url,
                                                   "--remote-db", "project", "--json",
                                                   "snapshot", "create"])
                need(snapshot.get("db_id") == db_a and snapshot.get("authority_rotated") is True,
                     "snapshot identity/authority mismatch")
                bundle = Path(snapshot["bundle"])
                need(bundle.is_dir() and (bundle / "database.db").is_file()
                     and (bundle / "manifest.json").is_file(), "native bundle incomplete")
                manifest = json.loads((bundle / "manifest.json").read_text(encoding="utf-8"))
                captured_at = manifest["captured_at"]
                need(isinstance(captured_at, int) and captured_at > 0,
                     "native snapshot manifest lacks a captured-at timestamp")
                still = a.call_tool("get", {"db": "project", "id": old["id"], "body": True})
                need("ALPHA_OLD_BODY" in still.get("body", ""), "owner failed to reopen after snapshot")
                changed = a.call_tool("ingest", {"db": "project", "summary": "ALPHA_NEW_SENTINEL library smoke", "body": "ALPHA_NEW_BODY post-snapshot"})
                need(changed.get("id"), "post-snapshot owner write failed")
                live_episode = a.call_tool("episode", {"db": "project", "action": "revise",
                    "episode_id": initial["a"]["episode_id"],
                    **episode_input("a-live", "Library smoke alpha corrected after snapshot", "ALPHA_EPISODE_LIVE_BODY",
                        expected_edition_id=snapshot_episode["edition_id"], reason="Record a later editorial correction.")})
            receipt["checks"].extend(["two_owner_ingest", "native_snapshot", "owner_reopen_and_new_write"])

            serving = root / "replica-serving"
            serving.mkdir()
            shutil.copy2(bundle / "database.db", serving / "database.db")
            if (bundle / "bodies").exists():
                shutil.copytree(bundle / "bodies", serving / "database.bodies")
            need((bundle / "database.db").is_file(), "serving copy mutated source bundle")
            replica = Host(mcp, root, "replica", ["--capability-profile", "read-only",
                           "--db", f"p_{snapshot['generation']}={serving / 'database.db'}"], env)
            hosts.append(replica)
            with McpClient(replica.url, timeout=30) as r:
                replica_db = one_db(r, f"p_{snapshot['generation']}")
                need(replica_db == db_a, "replica lost stable database identity")
                replica_row = r.call_tool("databases", {})[0]
                replica_resolved = replica_row["resolved_path"]
                need(Path(replica_resolved).is_absolute()
                     and Path(replica_resolved).resolve().is_relative_to(serving.resolve()),
                     f"replica resolved outside its serving generation: {replica_resolved}")
                denial = None
                try:
                    r.call_tool("ingest", {"db": f"p_{snapshot['generation']}", "summary": "MUTATION_DENIED"})
                except Exception as error:
                    denial = str(error)
                need(denial is not None, "read-only replica accepted mutation")
            receipt["checks"].append("replica_identity_and_read_only")

            config = root / "library.json"
            catalog = root / "catalog.json"
            write_json(catalog, {"schema": "mneme.library.catalog.v1", "library_id": "smoke-library",
                "revision": 1, "entries": [
                    {"project_id": "alpha", "db_id": db_a, "owner_device_id": "owner-a",
                     "display_name": "Alpha", "database": "project", "revision": 1,
                     "replicas": [{"source_device_id": "replica-a", "database": f"p_{snapshot['generation']}",
                                   "resolved_path": replica_resolved,
                                   "generation": snapshot["generation"], "captured_at": captured_at}]},
                    {"project_id": "beta", "db_id": db_b, "owner_device_id": "owner-b",
                     "display_name": "Beta", "database": "project", "revision": 1},
                ]})
            write_json(config, {"schema": "mneme.library.config.v1", "library_id": "smoke-library",
                "device_id": "coordinator", "catalog_path": "catalog.json",
                "rerank": options.rerank,
                "owners": {"owner-a": {"url": owner_a.url}, "owner-b": {"url": owner_b.url}},
                "replicas": {"replica-a": {"url": replica.url}}})
            if options.old_mcp:
                legacy = Host(options.old_mcp.resolve(), root, "old-context-owner", ["--capability-profile", "operator",
                              "--db", f"project={root / 'legacy.db'}"], env)
                hosts.append(legacy)
                with McpClient(legacy.url, timeout=30) as old_client:
                    legacy_id = one_db(old_client, "project")
                    old_client.call_tool("ingest", {"db": "project", "summary": "Library smoke legacy semantic note"})
                enrolled = json.loads(catalog.read_text())
                enrolled["entries"].append({"project_id": "legacy", "db_id": legacy_id, "owner_device_id": "owner-legacy",
                    "display_name": "Legacy context", "database": "project", "revision": 1})
                write_json(catalog, enrolled)
                routing = json.loads(config.read_text())
                routing["owners"]["owner-legacy"] = {"url": legacy.url}
                write_json(config, routing)
            started_query = time.monotonic()
            result = cli(mnemed, root, env, ["--json", "library", "--config", str(config),
                                             "query", "library smoke"])
            receipt["durations_ms"]["live_query"] = round((time.monotonic() - started_query) * 1000)
            need("probationary" not in result, "library query retained an obsolete probation lane")
            ids = {hit["id"] for hit in result["primary"]}
            need(old["id"] in ids and other["id"] in ids, "live library query missed a project")
            need(result["partial"] is bool(options.old_mcp)
                 and len(result["coverage"]) == (3 if options.old_mcp else 2),
                 "live library coverage did not mark incompatible prior owner partial")
            need(not ({initial["a"]["edition_id"], snapshot_episode["edition_id"], live_episode["edition_id"],
                       initial["b"]["edition_id"]} & ids), "diagnostic library_query leaked episodes")
            receipt["checks"].append("cli_two_live_sources")
            live_context = cli(mnemed, root, env, ["--json", "library", "--config", str(config),
                                                    "recall-context", "library smoke"])
            check_context(live_context, {"alpha": live_episode["edition_id"], "beta": initial["b"]["edition_id"]},
                          source_kind="live")
            need(not ({initial["a"]["edition_id"], snapshot_episode["edition_id"]}
                      & {card["id"] for card in live_context["episodes"]}), "live library context included stale editions")
            if options.old_mcp:
                legacy_coverage = next(item for item in live_context["coverage"] if item["project_id"] == "legacy")
                need(legacy_coverage["state"] == "refused"
                     and "mneme.context.v5" in legacy_coverage["reason"]
                     and live_context["partial"] is True
                     and not any(card["project_id"] == "legacy"
                                 for card in live_context["primary"] + live_context["episodes"]),
                     "v3 owner was not explicitly refused without leaking stale cards")
                need(not any(hit["project_id"] == "legacy" for hit in result["primary"]),
                     "incompatible old query leaked semantic cards")
                receipt["checks"].append("old_v3_owner_refused_without_cards")
            receipt["checks"].append("library_live_typed_current_episodes")
            if options.rerank:
                ranked = cli(mnemed, root, env, ["--json", "library", "--config", str(config),
                                                 "query", "Which project confirms completed field verification for the harbor library cart?"])
                need(ranked["ordering"] == "common_rerank" and ranked["ranker_semantics"],
                     "real common reranker did not admit pooled candidates")
                rank_ids = [hit["id"] for hit in ranked["primary"]]
                need(relevant["id"] in rank_ids and misleading["id"] in rank_ids,
                     "rank fixture candidates did not survive pooled retrieval")
                receipt["ranking"] = {"semantics": ranked["ranker_semantics"],
                                      "relevant_rank": rank_ids.index(relevant["id"]) + 1,
                                      "misleading_rank": rank_ids.index(misleading["id"]) + 1}
                need(receipt["ranking"]["relevant_rank"] < receipt["ranking"]["misleading_rank"],
                     "real common model ranked the unverified claim above confirmed verification")
                receipt["checks"].append("real_common_rerank_quality_fixture")

            coordinator = Host(mcp, root, "library-http", ["--library-config", str(config)], env)
            hosts.append(coordinator)
            with McpClient(coordinator.url, timeout=30, expected_server_name="mneme-mcp-library") as library:
                listing = library.list_tools()
                listed = {item["name"] for item in listing}
                need(listed == {"library_catalog", "library_query", "library_recall_context", "library_get"},
                     "library HTTP catalog exposed unexpected tools")
                need(library.call_tool("library_catalog", {})["library_id"] == "smoke-library",
                     "library HTTP catalog identity mismatch")
                http_context = library.call_tool("library_recall_context", {"text": "library smoke"})
                check_context(http_context, {"alpha": live_episode["edition_id"], "beta": initial["b"]["edition_id"]},
                              source_kind="live")
                try:
                    library.call_tool("ingest", {"summary": "MUTATION_DENIED"})
                except Exception:
                    pass
                else:
                    raise RuntimeError("library HTTP accepted owner mutator")
            stdio = StdioLibrary(mcp, config, root, env)
            stdio_list = stdio.request("tools/list", {})["tools"]
            need({item["name"] for item in stdio_list} == listed,
                 "library stdio/HTTP catalog mismatch")
            stdio.tool("library_catalog", {})
            stdio_context = stdio.tool("library_recall_context", {"text": "library smoke"})
            check_context(stdio_context, {"alpha": live_episode["edition_id"], "beta": initial["b"]["edition_id"]},
                          source_kind="live")
            for lane in ("primary", "episodes", "episodic_retrieval"):
                need(live_context[lane] == http_context[lane] == stdio_context[lane],
                     f"library CLI/HTTP/stdio mixed context differs in {lane}")
            receipt["checks"].append("library_cli_http_stdio_mixed_context_parity")
            denied = stdio.request("tools/call", {"name": "ingest", "arguments": {}})
            need(denied.get("isError") is True, "library stdio accepted owner mutator")
            receipt["checks"].append("library_http_stdio_closed_catalog")

            owner_a.stop()
            started_context = time.monotonic()
            context = cli(mnemed, root, env, ["--json", "library", "--config", str(config),
                                              "recall-context", "library smoke"])
            receipt["durations_ms"]["offline_fallback_context"] = round((time.monotonic() - started_context) * 1000)
            check_context(context, {"alpha": snapshot_episode["edition_id"]}, source_kind="snapshot",
                          generation=snapshot["generation"])
            need(live_episode["edition_id"] not in {card["id"] for card in context["episodes"]},
                 "offline fallback mixed in a post-snapshot episode edition")
            fallback = [hit for hit in context["primary"]
                        if hit["project_id"] == "alpha"]
            need(any(hit["id"] == old["id"] and hit["source"]["kind"] == "snapshot"
                     and hit["source"]["generation"] == snapshot["generation"] for hit in fallback),
                 "offline owner did not fall back to pinned old snapshot: " + json.dumps({
                     "old_id": old["id"], "coverage": context["coverage"],
                     "fallback": [{"id": hit["id"], "source": hit["source"]} for hit in fallback],
                 }, sort_keys=True))
            need(changed["id"] not in {hit["id"] for hit in fallback},
                 "fallback silently included a newer owner version")
            need(context["partial"] is True and any(c["state"] == "snapshot" for c in context["coverage"]),
                 "snapshot coverage not explicitly partial")
            got = cli(mnemed, root, env, ["--json", "library", "--config", str(config), "get",
                                             "alpha", old["id"], "replica-a", "--generation", snapshot["generation"]])
            need("ALPHA_OLD_BODY" in got["node"].get("body", ""),
                 "pinned library_get did not hydrate managed snapshot body")
            episode_got = cli(mnemed, root, env, ["--json", "library", "--config", str(config), "get",
                              "alpha", snapshot_episode["edition_id"], "replica-a", "--generation", snapshot["generation"]])
            need(episode_got["node"].get("body") == "ALPHA_EPISODE_SNAPSHOT_BODY"
                 and episode_got["node"]["memory_kind"]["kind"] == "episode"
                 and episode_got["node"]["memory_kind"]["episode"]["episode_id"] == initial["a"]["episode_id"],
                 "source-pinned episode get lost its edition body or facet")
            receipt["checks"].append("same_snapshot_mixed_lanes_and_pinned_episode_body")
            expired = cli(mnemed, root, env, ["library", "--config", str(config), "get",
                                                 "alpha", old["id"], "replica-a", "--generation", "expired"], ok=False)
            need("generation expired" in expired, "expired pinned ref was not refused")
            receipt["checks"].extend(["offline_snapshot_fallback_asof", "pinned_body_get", "expired_ref_refusal"])

            wrong = json.loads(catalog.read_text())
            wrong["entries"][0]["db_id"] = "WRONG_DATABASE_ID"
            write_json(catalog, wrong)
            mismatch = cli(mnemed, root, env, ["--json", "library", "--config", str(config),
                                               "query", "library smoke"])
            need(not any(h["project_id"] == "alpha" for h in mismatch["primary"]),
                 "wrong DB identity leaked snapshot results")
            need(mismatch["partial"] is True and any(c["project_id"] == "alpha" and c["state"] == "refused"
                                                   for c in mismatch["coverage"]),
                 "wrong DB identity did not mark refused partial coverage: "
                 + json.dumps(mismatch["coverage"], sort_keys=True))
            receipt["checks"].append("wrong_db_id_no_fallback")
        except Exception as error:
            receipt.update(status="failed", error=str(error)[-4000:],
                           logs={path.name: path.read_text(errors="replace")[-6000:] for path in root.glob("*.log")})
            write_json(output, receipt)
            raise
        finally:
            if stdio is not None:
                stdio.stop()
            for host in reversed(hosts):
                host.stop()
    receipt["status"] = "passed"
    write_json(output, receipt)
    print(json.dumps(receipt, sort_keys=True))


if __name__ == "__main__":
    main()
