#!/usr/bin/env python3
"""Frozen-fixture, evaluation-only cheap-reader probe.

The packet experiment is explicit-card selection, not retrieval. The native
experiment runs the unchanged collector and hook finalizer against disposable
stores. Neither path learns from labels, actor answers, or stored observations.
No provider is contacted without --live-reader or --run-actors.
"""
from __future__ import annotations

import argparse
from contextlib import contextmanager
import json
import os
from pathlib import Path
import subprocess
import sys
import tempfile
import time
from unittest.mock import patch

sys.dont_write_bytecode = True
ROOT = Path(__file__).resolve().parents[1]
sys.path.insert(0, str(ROOT / "tools"))
import reflexive_transfer_probe as transfer  # noqa: E402
import reflexive_reader as reader  # noqa: E402

CASES = ROOT / "tools/testdata/reflexive-reader-v1/cases.json"
ARMS = ("off", "all", "reader")
MAX_ACTOR_CALLS = 30


def need(ok: bool, message: str) -> None:
    if not ok:
        raise RuntimeError(message)


def checkpoint(path: Path, receipt: dict) -> None:
    temporary = path.with_name(path.name + ".tmp")
    temporary.write_text(json.dumps(receipt, indent=2, ensure_ascii=False) + "\n", encoding="utf-8")
    temporary.replace(path)


def inventory(document: dict, split: str) -> list[dict]:
    cases = transfer.case_inventory(document, split)
    need(len(document["cases"]) == 10 and len([c for c in document["cases"] if c["split"] == "development"]) == 5
         and len([c for c in document["cases"] if c["split"] == "holdout"]) == 5,
         "reader fixture must remain frozen at five development and five holdout cases")
    cards = {c["id"] for c in document["cards"]}
    for case in document["cases"]:
        need(len(case["stages"]) == 1, "reader cases must be single-stage")
        stage = case["stages"][0]
        host = stage["host"]
        packet = host.get("selector_packet")
        labels = host.get("expected_retention")
        need(isinstance(packet, list) and 0 < len(packet) <= 2 and len(set(packet)) == len(packet)
             and set(packet) <= cards, "invalid explicit selector packet")
        need(isinstance(labels, dict) and all(isinstance(labels.get(k), list) for k in ("keep", "drop"))
             and isinstance(labels.get("rationale"), str), "invalid retention labels")
        keep, drop, allow = (set(labels.get(k, [])) for k in ("keep", "drop", "allow"))
        need(not ((keep & drop) or (keep & allow) or (drop & allow))
             and keep | drop | allow <= cards, "overlapping or unknown retention labels")
    return cases


def retention(selected: list[str], available: list[str], labels: dict) -> dict:
    available_set, selected_set = set(available), set(selected)
    keep, drop, allow = (set(labels.get(k, [])) & available_set for k in ("keep", "drop", "allow"))
    need(selected_set <= available_set, "selector chose an unavailable card")
    return {"available_keep": sorted(keep), "available_drop": sorted(drop),
            "available_allow": sorted(allow), "kept": sorted(selected_set),
            "correct_keep": sorted(selected_set & keep),
            "missed_keep": sorted(keep - selected_set),
            "correct_drop": sorted(drop - selected_set),
            "wrong_keep": sorted(selected_set & drop),
            "exact_definite": keep <= selected_set and not (selected_set & drop)}


def select(cue: str, cards: list[dict], ledger: Path, live: bool, key_file: Path | None) -> dict:
    need(len(cue.encode("utf-8")) <= 800, "selector cue exceeds 800 bytes")
    projected = [{k: card[k] for k in ("id", "summary", "source", "fingerprint", "native")
                  if k in card} for card in cards]
    result = reader.select(cue, projected, ledger, live=live, key_file=key_file)
    need(isinstance(result, dict) and isinstance(result.get("selected_ids"), list)
         and len(result["selected_ids"]) == len(set(result["selected_ids"]))
         and set(result["selected_ids"]) <= {card["id"] for card in cards},
         "reader returned IDs outside admitted packet")
    return result


def packet_case(case: dict, lookup: dict, ledger: Path, live: bool,
                key_file: Path | None) -> dict:
    stage = case["stages"][0]
    packet = stage["host"]["selector_packet"]
    cards = [{"id": key, "summary": lookup[key]["text"]} for key in packet]
    cue = stage["scene"]["brief"]
    first = select(cue, cards, ledger, live, key_file)
    repeated = select(cue, cards, ledger, live, key_file)
    # Exact same task/question; only card order changes. The provider's q0/q1
    # identity then differs, so this must not be treated as a cache hit.
    reverse = select(cue, list(reversed(cards)), ledger, live, key_file)
    if live and first.get("provider_attempt") and first.get("reason") not in ("provider_error", "budget_exhausted"):
        need(repeated.get("cache_hit") and not repeated.get("provider_attempt"),
             "identical packet repeat missed the zero-network cache")
    labels = stage["host"]["expected_retention"]
    return {"case": case["id"], "split": case["split"], "packet_ids": packet,
            "first": first, "repeat": repeated, "reverse": reverse,
            "retention": retention(first["selected_ids"], packet, labels),
            "reverse_retention": retention(reverse["selected_ids"], packet, labels)}


@contextmanager
def recall_override(service_config: Path, project: Path, arm: str, native: list[dict],
                    ledger: Path, live: bool, key_file: Path | None):
    """Only insert selection after unchanged _collect, before hook finalization."""
    original_client = transfer.mcp_client.McpClient

    class Proxy(original_client):
        def call_tool(self, name, arguments):
            value = super().call_tool(name, arguments)
            if name == "recall_context":
                need(value.get("schema") == "mneme.context.v5" and "probationary" not in value,
                     "native collector returned an obsolete context lifecycle")
                need(arguments.get("depth") == 2 and arguments.get("observe") is True,
                     "native collector contract changed")
                native.append({"request": arguments,
                               "observation": value.get("observation"),
                               "lane_ids": {lane: [c["id"] for c in value.get(lane, [])]
                                            for lane in ("primary", "episodes")}})
            return value

    def collect(_config, cue):
        started = time.monotonic()
        with patch.object(transfer.hook_recall, "McpClient", Proxy):
            result = transfer.hook_recall._collect(service_config, cue, project, 1.5, observe=True)
        candidate_cards = list(result.get("cards", []))
        record = {"cue": cue[:800], "candidate_ids": [c["id"] for c in candidate_cards],
                  "native_empty": not candidate_cards, "native_outcome": result.get("outcome")}
        if arm == "reader" and result.get("outcome") == "ok" and candidate_cards:
            observed = result.get("observation")
            native_by_id = ({entry["node_id"]: entry for entry in observed["cards"]}
                            if isinstance(observed, dict) and isinstance(observed.get("cards"), list)
                            else {})
            selector_cards = [{**c, **({"native": native_by_id[c["id"]]}
                                    if c["id"] in native_by_id else {})} for c in candidate_cards]
            decision = select(cue[:800], selector_cards, ledger, live, key_file)
            selected = set(decision["selected_ids"])
            result["cards"] = [c for c in candidate_cards if c["id"] in selected]
            observation = result.get("observation")
            if isinstance(observation, dict) and isinstance(observation.get("cards"), list):
                result["observation"] = {**observation, "cards": [c for c in observation["cards"]
                    if c.get("node_id") in selected]}
            # Abstention is not a native empty result. The real hook sees ok + []
            # and makes its ordinary no-delivery state transition.
            record["selector"] = decision
            record["selector_abstained"] = not selected
        else:
            record["selector"] = None
            record["selector_abstained"] = False
        record["selected_ids"] = [c["id"] for c in result.get("cards", [])]
        record["elapsed_ms"] = int((time.monotonic() - started) * 1000)
        native.append({"collector": record})
        return result

    with patch.object(transfer.hooks, "_recall_cards", collect):
        yield


def native_context(project: Path, db: Path, root: Path, mcp: Path, env: dict,
                   arm: str, scene: dict, ledger: Path, live: bool,
                   key_file: Path | None) -> dict:
    if arm == "off":
        return {"context": "", "delivered": [], "native": [], "recall_outcome": "off",
                "selector_abstained": False, "native_empty": False, "elapsed_ms": 0}
    host = transfer.Host(mcp, root, "read-" + arm,
                         ["--capability-profile", "read-only", "--db", f"project={db}"], env)
    native: list[dict] = []
    try:
        service_config = root / f"service-{arm}.json"
        service_config.write_text(json.dumps({"binary": str(mcp), "project_db": str(db),
            "port": host.port, "state_dir": str(root / "service-state"),
            "working_directory": str(project)}), encoding="utf-8")
        config = {"project_root": project, "state_dir": root / "hook-state",
                  "service_config": service_config, "recall_mode": "automatic",
                  "memory_mode": "shadow", "memory_scope": "project"}
        session, turn = f"{root.name}-{arm}", "stage-1"
        event = {"hook_event_name": "UserPromptSubmit", "cwd": str(project),
                 "session_id": session, "turn_id": turn,
                 "prompt": "Decide the actions for this task: " + scene["brief"]}
        transfer.hooks.handle_event({"hook_event_name": "SessionStart", "cwd": str(project),
                                     "session_id": session, "source": "startup"}, config)
        started = time.monotonic()
        with patch.dict(os.environ, env):
            with recall_override(service_config, project, arm, native, ledger, live, key_file):
                value = transfer.hooks.handle_event(event, config)
        elapsed = int((time.monotonic() - started) * 1000)
        context = value.get("hookSpecificOutput", {}).get("additionalContext", "")
        need(len(context.encode()) <= transfer.hooks.MAX_CONTEXT_BYTES,
             "hook context over cap")
        state = json.loads((config["state_dir"] / (transfer.digest(session.encode()) + ".json")).read_text())
        turn_state = state["turns"][turn]
        delivered = turn_state["shadow"]["cards"]
        need(len(delivered) <= 2 and all(c["id"] in context for c in delivered),
             "hook delivery/state mismatch")
        collector = next((entry["collector"] for entry in native if "collector" in entry), None)
        return {"context": context, "context_bytes": len(context.encode()),
                "delivered": delivered, "delivered_ids": [c["id"] for c in delivered],
                "native": native, "cue": turn_state["shadow"]["cue"],
                "recall_outcome": turn_state["recall"]["outcome"],
                "selector_abstained": collector["selector_abstained"] if collector else False,
                "native_empty": collector["native_empty"] if collector else False,
                "error": collector is None or collector["native_outcome"] not in ("ok", "empty"),
                "elapsed_ms": elapsed}
    finally:
        host.stop()


def native_case(case: dict, lookup: dict, mnemed: Path, mcp: Path, codex: Path,
                actor_home: Path | None, base: Path, ledger: Path, live: bool,
                run_actors: bool, key_file: Path | None, actor_cache: dict,
                counters: dict) -> list[dict]:
    stage = case["stages"][0]
    for arm in ARMS:
        root = base / f"{case['id']}-{arm}"
        root.mkdir()
        env = transfer.fixture_env(root, mnemed)
        project, db, identifiers = transfer.fixture_case(case, lookup, arm, root, mnemed, env)
        edges = transfer.add_associations(case, arm, project, db, mnemed, env, identifiers)
        before = transfer.logical_cozo(db)
        recalled = native_context(project, db, root, mcp, env, arm,
                                  stage["scene"], ledger, live, key_file)
        need(transfer.logical_cozo(db) == before, "native recall changed logical Cozo KV")
        prompt = transfer.actor_prompt(stage["scene"], recalled["context"]).encode("utf-8")
        actor = None
        reused_from = None
        if run_actors:
            if prompt in actor_cache:
                source, actor = actor_cache[prompt]
                reused_from = source
                counters["actor_reuses"] += 1
            else:
                need(counters["actor_calls"] < MAX_ACTOR_CALLS, "actor call cap reached")
                actor = transfer.actor_call(codex, actor_home, project, env,
                                            stage["scene"], recalled["context"])
                counters["actor_calls"] += 1
                actor_cache[prompt] = (f"{case['id']}:{arm}", actor)
        grade = (transfer.score(actor["answer"]["actions"], stage["host"],
                                stage["scene"]["commands"])
                 if actor and actor.get("status") == "answered" else None)
        need(transfer.logical_cozo(db) == before, "actor changed logical Cozo KV")
        collector = next((entry["collector"] for entry in recalled["native"]
                          if "collector" in entry), None)
        reverse_ids = {value: key for key, value in identifiers.items()}
        labels = stage["host"]["expected_retention"]
        candidate_ids = collector["candidate_ids"] if collector else []
        selected_ids = collector["selected_ids"] if collector else []
        yield {"case": case["id"], "split": case["split"], "arm": arm,
               "seeded_ids": identifiers, "edges": edges, "recall": recalled,
               "native_candidate_fixture_ids": [reverse_ids.get(x) for x in candidate_ids],
               "native_selected_fixture_ids": [reverse_ids.get(x) for x in selected_ids],
               "native_retention": retention([reverse_ids[x] for x in selected_ids if x in reverse_ids],
                                             [reverse_ids[x] for x in candidate_ids if x in reverse_ids],
                                             labels) if arm == "reader" else None,
               "actor": actor, "reused_from": reused_from, "grade": grade,
               "persistent_kv_equal": True}


def totals(rows: list[dict]) -> dict:
    decisions = []
    actors = []
    for row in rows:
        if "first" in row:
            decisions.extend(row[key] for key in ("first", "repeat", "reverse"))
        if "recall" in row:
            for entry in row["recall"]["native"]:
                if "collector" in entry and entry["collector"]["selector"]:
                    decisions.append(entry["collector"]["selector"])
            if row.get("actor") and row.get("reused_from") is None:
                actors.append(row["actor"])
    attempted = [d for d in decisions if d.get("provider_attempt")]
    return {"total_provider_attempts": len(attempted) + len(actors),
            "reader_provider_attempts": len(attempted),
            "reader_cache_hits": sum(bool(d.get("cache_hit")) for d in decisions),
            # Reservation can be durable even if no network attempt began
            # (for example a malformed key after reserve-first admission).
            "reader_reservations_usd": sum(float(d.get("reserved_usd") or 0) for d in decisions),
            "reader_estimated_usd_known": sum(float(d["estimated_usd"]) for d in attempted
                                               if d.get("usage") is not None and d.get("estimated_usd") is not None),
            "reader_unknown_usage_attempts": sum(d.get("usage") is None for d in attempted),
            "reader_usage": [d["usage"] for d in attempted if d.get("usage") is not None],
            "actor_provider_attempts": len(actors),
            "actor_reused_answers": sum(row.get("reused_from") is not None for row in rows),
            "actor_usage": [a["usage"] for a in actors if a.get("usage") is not None],
            "actor_unknown_usage_attempts": sum(a.get("usage") is None for a in actors),
            "actor_elapsed_ms": sum(a.get("elapsed_ms", 0) for a in actors),
            "reader_attempt_latency_ms": sum(float(d.get("latency_ms") or 0) for d in attempted),
            "native_hook_elapsed_ms": sum(row["recall"].get("elapsed_ms", 0)
                                          for row in rows if "recall" in row)}


def receipt_for(args, cases_path: Path, document: dict, cases: list[dict],
                mnemed: Path, mcp: Path, codex: Path) -> dict:
    sources = {name: transfer.artifact(ROOT / path) for name, path in {
        "runner": "tools/reflexive_reader_probe.py",
        "reader": "tools/reflexive_reader.py",
        "transfer_baseline": "tools/reflexive_transfer_probe.py",
        "reader_http": "tools/selection_http.py",
        "protocol": "tools/testdata/README.md",
        "runner_tests": "tools/test_reflexive_reader_probe.py",
        "reader_tests": "tools/test_reflexive_reader.py",
        "collector": "integrations/codex/hook_recall.py",
        "hook": "integrations/codex/hooks.py",
        "client": "integrations/codex/mcp_client.py"}.items()}
    binaries = {}
    if args.phase == "native":
        binaries.update(mnemed=transfer.artifact(mnemed), mcp=transfer.artifact(mcp))
    if args.run_actors:
        binaries["codex"] = transfer.artifact(codex)
    embedding = None
    if args.phase == "native":
        model_root = transfer.CACHE / "models--Xenova--bge-base-en-v1.5"
        revision_file = model_root / "refs/main"
        need(revision_file.is_file(), "offline neural model revision unavailable")
        revision = revision_file.read_text(encoding="utf-8").strip()
        model_blob = model_root / "snapshots" / revision / "onnx/model.onnx"
        need(model_blob.is_file(), "offline neural model blob unavailable")
        embedding = {"backend": "cached BGE 768 offline", "rerank": "disabled",
                     "revision": revision, "blob": transfer.artifact(model_blob)}
    return {"schema": "mneme.reflexive-reader-probe.v1", "status": "prepared",
            "phase": args.phase, "split": args.split, "case_file": transfer.artifact(cases_path),
            "source_commit": subprocess.run(["git", "rev-parse", "HEAD"], cwd=ROOT,
                text=True, capture_output=True, check=True).stdout.strip(),
            "sources": sources, "binaries": binaries, "embedding": embedding,
            "selector": {"live": args.live_reader, "ledger": str(args.ledger) if args.live_reader else None},
            "actors": {"run": args.run_actors, "model": transfer.MODEL,
                       "effort": "low", "max_actual_calls": MAX_ACTOR_CALLS},
            "case_ids": [c["id"] for c in cases],
            "limits": {"cue_bytes": 800, "admitted_cards": 2, "native_depth": 2,
                       "native_k": 2, "native_max_nodes": 4},
            "caveats": ["evaluation-only; production hook unchanged",
                        "same-prompt actor answers reused only within a case",
                        "no assessor calls, training, or learning"],
            "results": [], "actual_calls": {"actor_calls": 0, "actor_reuses": 0}}


def main() -> None:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--phase", choices=("packet", "native"), default="packet")
    parser.add_argument("--split", choices=("development", "holdout", "all"), default="development")
    parser.add_argument("--cases", type=Path, default=CASES)
    parser.add_argument("--mnemed", type=Path, default=transfer.BIN / "mnemed")
    parser.add_argument("--mcp", type=Path, default=transfer.BIN / "mneme-mcp")
    parser.add_argument("--codex", type=Path, default=transfer.CODEX)
    parser.add_argument("--ledger", type=Path)
    parser.add_argument("--key-file", type=Path)
    parser.add_argument("--live-reader", action="store_true")
    parser.add_argument("--run-actors", action="store_true")
    parser.add_argument("--output", required=True, type=Path)
    args = parser.parse_args()
    need(not args.run_actors or args.phase == "native", "actors require native phase")
    need(not args.live_reader or (args.key_file is not None and args.ledger is not None),
         "live reader requires explicit --key-file and shared --ledger")
    need(args.phase != "native" or args.split == "development" or (args.live_reader and args.run_actors),
         "holdout native preflight forbidden; use both --live-reader and --run-actors")
    cases_path, mnemed, mcp, codex = (p.expanduser().resolve() for p in
                                      (args.cases, args.mnemed, args.mcp, args.codex))
    output = args.output.expanduser().resolve()
    need(not output.exists(), "refusing to overwrite existing output")
    need(cases_path.is_file(), "fixture file missing")
    if args.phase == "native":
        need(mnemed.is_file() and mcp.is_file(), "frozen native binaries missing")
    if args.run_actors:
        need(codex.is_file() and os.access(codex, os.X_OK), "actor Codex binary missing")
    if args.live_reader:
        args.key_file = args.key_file.expanduser().resolve()
        need(args.key_file.is_file(), "reader key file missing")
    args.ledger = (args.ledger or output.with_suffix(".reader-ledger.json")).expanduser().resolve()
    document = json.loads(cases_path.read_text(encoding="utf-8"))
    cases = inventory(document, args.split)
    receipt = receipt_for(args, cases_path, document, cases, mnemed, mcp, codex)
    output.parent.mkdir(parents=True, exist_ok=True)
    checkpoint(output, receipt)
    lookup = {card["id"]: card for card in document["cards"]}
    if args.phase == "packet":
        if args.live_reader:
            for case in cases:
                receipt["results"].append(packet_case(case, lookup, args.ledger, True, args.key_file))
                receipt["totals"] = totals(receipt["results"])
                checkpoint(output, receipt)
    else:
        receipt["status"] = "running"
        with tempfile.TemporaryDirectory(prefix="mneme-reader-probe-") as name:
            base = Path(name).resolve()
            actor_home = None
            if args.run_actors:
                auth = Path(os.environ.get("CODEX_HOME", Path.home() / ".codex")) / "auth.json"
                need(auth.is_file(), "Codex auth unavailable")
                actor_home = base / "codex-home"
                actor_home.mkdir(mode=0o700)
                (actor_home / "auth.json").symlink_to(auth)
            for case in cases:
                cache: dict = {}
                # Checkpoint each arm; a failed later arm must not erase evidence.
                for row in native_case(case, lookup, mnemed, mcp, codex, actor_home,
                                       base, args.ledger, args.live_reader, args.run_actors,
                                       args.key_file, cache, receipt["actual_calls"]):
                    receipt["results"].append(row)
                    receipt["totals"] = totals(receipt["results"])
                    checkpoint(output, receipt)
    receipt["status"] = "completed" if (args.phase == "native" or args.live_reader) else "prepared"
    receipt["totals"] = totals(receipt["results"])
    checkpoint(output, receipt)
    print(json.dumps({"status": receipt["status"], "output": str(output),
                      "rows": len(receipt["results"]), "totals": receipt["totals"]}))


if __name__ == "__main__":
    main()
