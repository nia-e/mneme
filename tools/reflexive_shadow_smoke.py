#!/usr/bin/env python3
"""Finite disposable installed-bundle smoke for observation-only Codex shadow mode.

No build, live store, global Codex configuration, or provider call by default.
Pass --run-assessor with an explicit --codex only when a paid model call is intended.
The receipt is a compact evidence index, not a transcript.
"""

from __future__ import annotations

import argparse
from contextlib import closing
import hashlib
import json
import os
from pathlib import Path
import re
import socket
import sqlite3
import subprocess
import sys
import tempfile
import time

sys.dont_write_bytecode = True
REPO = Path(__file__).resolve().parents[1]
sys.path.insert(0, str(REPO / "integrations" / "codex"))
from mcp_client import McpClient, McpError  # noqa: E402
from memory_library_smoke import Host  # noqa: E402
from capture_links_smoke import Stdio  # noqa: E402
import install  # noqa: E402


def need(condition: bool, message: str) -> None:
    if not condition:
        raise RuntimeError(message)


def sha(path: Path) -> str:
    with path.open("rb") as stream:
        return hashlib.file_digest(stream, "sha256").hexdigest()


def artifact(path: Path) -> dict:
    return {"path": str(path), "bytes": path.stat().st_size, "sha256": sha(path)}


def invoke(argv: list[str | Path], *, cwd: Path, env: dict, payload: dict | None = None,
           timeout: float = 60, ok: bool = True) -> dict | str:
    result = subprocess.run([str(x) for x in argv], cwd=cwd, env=env, text=True,
                            input=json.dumps(payload) if payload is not None else None,
                            capture_output=True, timeout=timeout)
    need((result.returncode == 0) == ok,
         f"{Path(str(argv[0])).name} {list(map(str, argv[1:]))!r} exit={result.returncode}: {result.stderr[-800:]}")
    if not ok:
        return result.stderr
    return json.loads(result.stdout)


def free_port() -> int:
    with socket.socket() as sock:
        sock.bind(("127.0.0.1", 0))
        return sock.getsockname()[1]


def logical_cozo(path: Path) -> dict:
    """Hash the entire persistent Cozo KV relation in one read-only transaction.

    Unlike file hashes this ignores SQLite page/WAL churn; unlike public get it
    includes hidden node interference, feedback retry rows, and edge counters.
    """
    uri = "file:" + str(path) + "?mode=ro"
    with closing(sqlite3.connect(uri, uri=True, timeout=5)) as db:
        db.execute("BEGIN")
        schema = db.execute("SELECT name, sql FROM sqlite_master WHERE type='table' ORDER BY name").fetchall()
        need([name for name, _ in schema] == ["cozo"], f"unexpected Cozo SQLite schema: {schema!r}")
        digest = hashlib.sha256()
        rows = 0
        for key, value in db.execute("SELECT k, v FROM cozo ORDER BY k"):
            for blob in (key, value):
                raw = bytes(blob)
                digest.update(len(raw).to_bytes(8, "big"))
                digest.update(raw)
            rows += 1
        db.rollback()
    return {"rows": rows, "sha256": digest.hexdigest()}


def cli(binary: Path, project: Path, env: dict, *args: str, payload: dict | None = None,
        db: Path | None = None):
    return invoke([binary, "--db", db or project / ".mneme/codex-memory.db", "--json", *args],
                  cwd=project, env=env, payload=payload, timeout=90)


def public_snapshot(binary: Path, project: Path, env: dict, ids: list[str]) -> dict:
    return {"status": cli(binary, project, env, "status"),
            "nodes": {identifier: cli(binary, project, env, "get", identifier, "--body", "--edges")
                      for identifier in ids}}


def event(project: Path, kind: str, session: str, turn: str | None = None,
          prompt: str | None = None, answer: str | None = None) -> dict:
    row = {"hook_event_name": kind, "cwd": str(project), "session_id": session}
    if kind == "SessionStart":
        row["source"] = "startup"
    if turn is not None:
        row["turn_id"] = turn
    if prompt is not None:
        row["prompt"] = prompt
    if answer is not None:
        row["last_assistant_message"] = answer
    if kind == "Stop":
        row["stop_hook_active"] = False
    return row


def hook(script: Path, config: Path, project: Path, env: dict, payload: dict) -> dict:
    return invoke([sys.executable, script, "--config", config], cwd=project,
                  env=env, payload=payload, timeout=7)


def fake_codex(path: Path) -> None:
    # This executable simulates only the assessor's structured-output contract.
    # It records the actual isolated exec argv without ever contacting a provider.
    path.write_text(f"#!{sys.executable}\n" + '''import json, os, pathlib, sys
args = sys.argv[1:]
required = ('exec', '--ignore-user-config', '--ephemeral', '--sandbox', 'read-only',
            '--disable', 'hooks', '--output-schema', '-o', '-m', 'gpt-5.6-sol')
if any(x not in args for x in required):
    raise SystemExit(3)
raw = sys.stdin.read()
case = json.loads(raw.split('Case: ', 1)[1])
rows = [{'id': c['id'], 'outcome': 'unknown', 'evidence': 'bounded record does not show added value'}
        for c in case['delivered_cards']]
pathlib.Path(args[args.index('-o') + 1]).write_text(json.dumps({'judgments': rows}))
trace = os.environ.get('MNEME_SHADOW_TRACE')
if trace:
    pathlib.Path(trace).write_text(json.dumps({'argv': args, 'card_ids': [c['id'] for c in case['delivered_cards']]}))
print(json.dumps({'type': 'turn.completed'}))
''', encoding="utf-8")
    path.chmod(0o700)


def wait_sidecar(path: Path, timeout: float = 55) -> dict:
    deadline = time.monotonic() + timeout
    while time.monotonic() < deadline:
        if path.is_file():
            return json.loads(path.read_text(encoding="utf-8"))
        time.sleep(0.1)
    raise RuntimeError(f"shadow sidecar did not arrive: {path}")


def main() -> None:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--mnemed", required=True, type=Path)
    parser.add_argument("--mcp", required=True, type=Path)
    parser.add_argument("--codex", type=Path, help="explicit Codex executable for --run-assessor")
    parser.add_argument("--run-assessor", action="store_true", help="authorize one actual Codex model call")
    parser.add_argument("--output", required=True, type=Path)
    args = parser.parse_args()
    need(not args.run_assessor or args.codex is not None,
         "--run-assessor requires an explicit --codex executable")
    need(args.run_assessor or args.codex is None,
         "--codex without --run-assessor is ambiguous; omit it for deterministic stub")
    binaries = [args.mnemed.resolve(), args.mcp.resolve()]
    for binary in binaries + ([args.codex.resolve()] if args.codex else []):
        need(binary.is_file() and os.access(binary, os.X_OK), f"missing executable: {binary}")
    output = args.output.resolve()
    need(output not in binaries, "output cannot overwrite a binary")
    output.parent.mkdir(parents=True, exist_ok=True)
    receipt = {"schema": "mneme.reflexive-shadow-installed-smoke.v1", "status": "running",
               "mode": "actual_assessor" if args.run_assessor else "deterministic_stub",
               "source_commit": subprocess.run(["git", "rev-parse", "HEAD"], cwd=REPO,
                   text=True, capture_output=True, check=True).stdout.strip(),
               "harness": artifact(Path(__file__).resolve()),
               "sources": {"mnemed": artifact(binaries[0]), "mneme-mcp": artifact(binaries[1])},
               "checks": []}
    with tempfile.TemporaryDirectory(prefix="mneme-reflexive-shadow-") as name:
        # macOS /var is a symlink to /private/var. All generated project/store
        # identities must use one canonical spelling for the hook boundary.
        root = Path(name).resolve()
        project = root / "project"
        project.mkdir()
        db = project / ".mneme/codex-memory.db"
        db.parent.mkdir()
        prefix = root / "private-install"
        env = os.environ.copy()
        env.pop("MNEME_DB", None)
        env.update({"MNEME_RERANK": "0", "GIT_CEILING_DIRECTORIES": str(root),
                    "HF_HUB_OFFLINE": "1", "TRANSFORMERS_OFFLINE": "1",
                    "MNEME_SHADOW_TRACE": str(root / "assessor-trace.json")})
        os.environ["MNEME_CLIENT_BINARY"] = str(prefix / "bin/mnemed")
        ids = []
        cli(binaries[0], project, env, "capture", "init", db=db)
        for key, summary in (
            ("tunnel", "The northern tunnel floods when the upstream sluice is open; close the sluice before digging."),
            ("house", "Keep the starter house intact; the return route is marked beside its east wall."),
        ):
            payload = {"source": {"namespace": "shadow-smoke", "key": key,
                                  "reference": f"smoke://reflexive-shadow/{key}"},
                       "summary": summary, "body": f"Disposable source body for {key}"}
            if key == "house":
                payload["links"] = [{"to": ids[0], "kind": "transition", "weight": 0.7}]
            ids.append(cli(binaries[0], project, env, "capture", "add", "--input", "-",
                           payload=payload, db=db)["id"])
        # Fresh single-graph stores already admit semantic captures and episodes.
        need(db.is_file(), "single-graph capture init did not create the store")
        receipt["checks"].append("fresh_single_graph_episode_ready")
        episode = {"source": {"namespace": "shadow-smoke", "key": "flood-scene",
                              "reference": "smoke://reflexive-shadow/flood-scene"},
                   "summary": "We discovered the flooded northern tunnel after leaving the sluice open.",
                   "body": "The first excavation flooded; next time close the sluice first.",
                   "thread": "tunnel-route", "occurred": {"kind": "point", "at": 1790000000000}}
        ids.append(cli(binaries[0], project, env, "episode", "append", "--input", "-",
                       payload=episode)["edition_id"])
        before_public = public_snapshot(binaries[0], project, env, ids)
        codex = args.codex.resolve() if args.run_assessor else root / "fake-codex"
        if not args.run_assessor:
            fake_codex(codex)
        plan = install.prepare(project, prefix, binaries[0], binaries[1], free_port(),
                               recall_mode="shadow", shadow_model="gpt-5.6-sol",
                               shadow_codex=codex)
        applied = install.apply(plan)
        receipt["install"] = {"plan_sha256": plan["plan_sha256"],
                              "config_revision": plan["config_revision"],
                              "files": {row["destination"]: sha(prefix / row["destination"])
                                        for row in plan["files"]},
                              "all_hashes_match": all(sha(prefix / row["destination"]) == row["sha256"]
                                                      for row in plan["files"])}
        need(receipt["install"]["all_hashes_match"] and len(plan["files"]) == len(install.DESTINATIONS),
             "private bundle hash or file-set mismatch")
        service_config = prefix / "config/service.json"
        service = prefix / "lib/service.py"
        hooks = prefix / "lib/hooks.py"
        hook_config = prefix / "config/hooks.json"
        service_started = False
        try:
            invoke([sys.executable, service, "--config", service_config, "start"], cwd=project,
                   env=env, timeout=40)
            service_started = True
            url = f"http://127.0.0.1:{plan['port']}/"
            with McpClient(url, timeout=30) as client:
                listed = {row["name"] for row in client.list_tools()}
                need("recall_context" in listed, "installed HTTP catalog missing recall_context")
                cue = "Investigate the flooded northern tunnel and preserve the starter house."
                arguments = {"db": "project", "text": cue, "k": 2, "max_nodes": 4, "depth": 2}
                plain = client.call_tool("recall_context", arguments)
                observed = client.call_tool("recall_context", {**arguments, "observe": True})
                need(plain.get("schema") == observed.get("schema") == "mneme.context.v5"
                     and "probationary" not in plain and "probationary" not in observed,
                     "native observation used an obsolete context lifecycle")
                for lane in ("primary", "episodes"):
                    need(plain[lane] == observed[lane], f"observe changed native {lane} cards")
                side = observed.get("observation")
                need(observed.get("receipt") is None and side.get("schema") == 1
                     and side.get("learning") == "disabled" and isinstance(side.get("cards"), list),
                     "native observation minted authority or malformed sidecar")
                for row in side["cards"]:
                    need(set(row) == {"node_id", "lane", "card_sha256", "graph_path"}
                         and re.fullmatch(r"[0-9a-f]{64}", row["card_sha256"]),
                         "native observation card contract changed")
                    cards = observed["episodes" if row["lane"] == "episodic" else row["lane"]]
                    card = next((c for c in cards if c["id"] == row["node_id"]), None)
                    need(card is not None, "observation named a card not in the emitted lane")
                    canonical = json.dumps(card, sort_keys=True, ensure_ascii=False,
                                           separators=(",", ":")).encode()
                    need(hashlib.sha256(canonical).hexdigest() == row["card_sha256"],
                         "native observation digest did not bind final emitted card JSON")
                receipt["native"] = {"card_ids": [r["node_id"] for r in side["cards"]],
                                     "schema": side["schema"], "learning": side["learning"],
                                     "receipt_null": True}
                receipt["checks"].append("http_observe_same_cards_non_authorizing")
                graph = client.call_tool("recall_context", {"db": "project",
                    "text": "starter house", "k": 1, "max_nodes": 4,
                    "depth": 2, "observe": True})
                paths = [(row["node_id"], row["graph_path"])
                         for row in graph["observation"]["cards"] if row["graph_path"]]
                need(any(node == ids[0] and path[0]["previous"] == ids[1]
                         and path[0]["target"] == ids[0]
                         and path[0]["from"] == ids[1]
                         and path[0]["to"] == ids[0]
                         and path[0]["kind"] == "transition" for node, path in paths),
                     f"native graph provenance did not witness house→tunnel stored edge: {paths!r}")
                receipt["native"]["graph_path"] = {"from": ids[1], "to": ids[0],
                                                      "kind": "transition", "depth": 2}
                receipt["checks"].append("http_observe_real_stored_graph_path")
            # The operator host still owns the lease. SQLite's read-only transaction
            # gives a logical KV baseline without calling an offline CLI.
            before_kv = logical_cozo(db)
            config = json.loads(hook_config.read_text())
            need(config["schema"] == "mneme.codex-hooks.config.v3" and config["memory_mode"] == "shadow",
                 "installed hook config is not shadow v3")
            session, turn = "shadow-smoke-session", "turn-1"
            start = hook(hooks, hook_config, project, env, event(project, "SessionStart", session))
            prompt = "Investigate the flooded northern tunnel and preserve the starter house."
            recalled = hook(hooks, hook_config, project, env,
                            event(project, "UserPromptSubmit", session, turn, prompt))
            context = recalled.get("hookSpecificOutput", {}).get("additionalContext", "")
            delivered = re.findall(r'"id":"([0-7][0-9A-HJKMNP-TV-Z]{25})"', context)
            state_files = list(Path(config["state_dir"]).glob("*.json"))
            debug_state = json.loads(state_files[0].read_text()) if state_files else None
            need(start == {} and delivered and set(delivered) <= set(ids),
                 f"installed prompt hook did not deliver seeded cards: start={start!r} recalled={recalled!r} seeded={ids!r} state={debug_state!r}")
            duplicate = hook(hooks, hook_config, project, env,
                             event(project, "UserPromptSubmit", session, turn, prompt))
            need(duplicate == {}, "duplicate prompt hook redelivered cards")
            answer = "The sluice being open explains the flood; close it before digging. Leave the starter house intact."
            stop = hook(hooks, hook_config, project, env,
                        event(project, "Stop", session, turn, answer=answer))
            repeated_stop = hook(hooks, hook_config, project, env,
                                 event(project, "Stop", session, turn, answer=answer))
            need(stop == repeated_stop == {}, "Stop must be silent and idempotent")
            turn_key = hashlib.sha256((session + "\0" + turn).encode()).hexdigest()
            sidecar = Path(config["state_dir"]) / "shadow" / (turn_key + ".json")
            assessed = wait_sidecar(sidecar)
            need(assessed.get("schema") == "mneme.codex-shadow.v1"
                 and assessed.get("turn_key") == turn_key
                 and {r["id"] for r in assessed.get("delivered", [])} == set(delivered),
                 "shadow sidecar did not bind delivered cards")
            if not args.run_assessor:
                need(assessed.get("status") == "observed"
                     and all(j["outcome"] == "unknown" for j in assessed["judgments"]),
                     "deterministic assessor should only emit unknown judgments")
                trace = json.loads((root / "assessor-trace.json").read_text())
                need(set(trace["card_ids"]) == set(delivered)
                     and "--ignore-user-config" in trace["argv"]
                     and "read-only" in trace["argv"], "isolated assessor invocation mismatch")
            receipt["hook"] = {"delivered_ids": delivered, "sidecar_status": assessed.get("status"),
                               "judgment_outcomes": [r["outcome"] for r in assessed.get("judgments", [])],
                               "provider_calls": 1 if args.run_assessor else 0}
            receipt["checks"].extend(["installed_prompt_delivery_and_dedupe", "stop_async_sidecar_once"])
            after_kv = logical_cozo(db)
            need(after_kv == before_kv,
                 f"shadow observation changed persistent Cozo KV: {before_kv} != {after_kv}")
            receipt["persistent_kv"] = {"before": before_kv, "after": after_kv,
                                        "equal": True,
                                        "scope": "all Cozo keys and values, including hidden weights and feedback ledger"}
            receipt["checks"].append("full_persistent_cozo_kv_unchanged")
            invoke([sys.executable, service, "--config", service_config, "stop"], cwd=project,
                   env=env, timeout=50)
            service_started = False
            after_public = public_snapshot(prefix / "bin/mnemed", project, env, ids)
            need(before_public == after_public, "public node/edge/status logical readback changed")
            receipt["checks"].append("public_node_edge_status_unchanged")
            # A second host on the same disposable store proves the read-only
            # profile has neither catalog nor raw dispatch authority for writes.
            readonly = Host(prefix / "bin/mneme-mcp", root, "readonly-http",
                            ["--capability-profile", "read-only", "--db", f"project={db}"], env)
            try:
                with McpClient(readonly.url, timeout=30) as client:
                    names = {row["name"] for row in client.list_tools()}
                    need("recall_context" in names and not ({"capture", "feedback", "reflect", "link", "forget"} & names),
                         "read-only HTTP catalog exposed a mutator")
                    try:
                        client.raw_rpc("tools/call", {"name": "feedback", "arguments":
                            {"db": "project", "to": ids[0], "signal": "irrelevant"}})
                    except McpError:
                        pass
                    else:
                        raise RuntimeError("read-only HTTP raw dispatch accepted feedback")
                    ro_context = client.call_tool("recall_context", {**arguments, "observe": True})
                    need(ro_context.get("observation", {}).get("learning") == "disabled",
                         "read-only HTTP observation unavailable")
            finally:
                readonly.stop()
            stdio = Stdio(prefix / "bin/mneme-mcp", root, env, db, "read-only")
            try:
                stdio_names = {row["name"] for row in stdio.request("tools/list", {})["tools"]}
                need(stdio_names == names, "stdio/HTTP read-only catalog mismatch")
                denial = stdio.tool("feedback", {"db": "project", "to": ids[0],
                                                 "signal": "irrelevant"}, ok=False)
                need(denial, "stdio raw feedback dispatch did not refuse")
                stdio_context = stdio.tool("recall_context", {**arguments, "observe": True})
                for lane in ("primary", "episodes"):
                    need(stdio_context[lane] == ro_context[lane],
                         f"stdio/HTTP read-only {lane} cards differ")
            finally:
                stdio.stop()
            need(logical_cozo(db) == before_kv, "read-only transport probes changed persistent store")
            receipt["checks"].append("read_only_stdio_http_catalog_dispatch_and_observe")
            receipt["status"] = "passed"
        finally:
            if service_started:
                try:
                    invoke([sys.executable, service, "--config", service_config, "stop"],
                           cwd=project, env=env, timeout=50)
                except Exception as exc:
                    receipt["cleanup_error"] = repr(exc)
            try:
                receipt["uninstall"] = install.uninstall(prefix / "receipt.json")
                need(not any((project / ".codex" / name).exists()
                             for name in ("config.toml", "hooks.json")),
                     "private project Codex config survived uninstall")
            except Exception as exc:
                receipt["uninstall_error"] = repr(exc)
        need("cleanup_error" not in receipt and "uninstall_error" not in receipt,
             "private cleanup failed")
    output.write_text(json.dumps(receipt, indent=2, sort_keys=True) + "\n", encoding="utf-8")
    print(json.dumps({"receipt": str(output), "status": receipt["status"],
                      "checks": len(receipt["checks"]), "provider_calls": receipt["hook"]["provider_calls"]}))


if __name__ == "__main__":
    main()
