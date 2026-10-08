#!/usr/bin/env python3
"""Disposable multi-turn evaluation of an agentic reader; never a product hook.

Default invocation only freezes a preparation receipt. Development-only native
preflight is explicit. Live reader and task actors have separate explicit flags.
The holdout has no native preflight path. No fixture labels or actor answers are
shown to the reader; no feedback or learning is performed.
"""
from __future__ import annotations

import argparse
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
import codex_reader as reader  # noqa: E402

CASES = ROOT / "tools/testdata/codex-reader-v1/cases.json"
ARMS = ("off", "all", "reader")
MAX_POOL = 6
MAX_READER_CARDS = 2
MAX_ACTOR_CALLS = 54
MAX_DIALOGUE_BYTES = 4000
MAX_ACTOR_MEMORY_BYTES = 12000
ACTOR_MODEL = transfer.MODEL


def need(condition: bool, message: str) -> None:
    if not condition:
        raise RuntimeError(message)


def bounded(text: str, limit: int) -> str:
    raw = text.encode("utf-8")
    return raw[-limit:].decode("utf-8", "ignore") if len(raw) > limit else text


def checkpoint(path: Path, receipt: dict) -> None:
    temp = path.with_name(path.name + ".tmp")
    temp.write_text(json.dumps(receipt, indent=2, ensure_ascii=False) + "\n", encoding="utf-8")
    temp.replace(path)


def inventory(document: dict, split: str) -> list[dict]:
    cases = transfer.case_inventory(document, split)
    need(split in ("development", "holdout") and len(document["cases"]) == 8
         and {s: sum(c["split"] == s for c in document["cases"])
              for s in ("development", "holdout")} == {"development": 4, "holdout": 4},
         "fixture must have four development and four holdout cases")
    for case in document["cases"]:
        need(2 <= len(case["stages"]) <= 3, "multi-turn case needs two or three stages")
        for stage in case["stages"]:
            delta = stage.get("dialogue_delta")
            need(isinstance(delta, list) and delta
                 and all(isinstance(d, dict) and set(d) == {"role", "text"}
                         and d["role"] in ("user", "assistant")
                         and isinstance(d["text"], str) and d["text"] for d in delta),
                 "invalid scripted dialogue delta")
            labels = stage["host"].get("expected_retention")
            need(isinstance(labels, dict) and all(isinstance(labels.get(k), list)
                  for k in ("keep", "drop")) and isinstance(labels.get("rationale"), str),
                 "invalid host retention labels")
            keep, drop, allow = (set(labels.get(k, [])) for k in ("keep", "drop", "allow"))
            card_ids = {card["id"] for card in document["cards"]}
            need(not (keep & drop or keep & allow or drop & allow)
                 and keep | drop | allow <= card_ids,
                 "overlapping retention labels")
    return cases


def dialogue_text(messages: list[dict]) -> str:
    full = "\n".join(f"{m['role']}: {m['text']}" for m in messages[-6:])
    return bounded(full, MAX_DIALOGUE_BYTES)


def hook_prompt(stage: dict, previous_messages: list[dict]) -> str:
    previous = dialogue_text(previous_messages)
    return ("Decide actions for the CURRENT task: " + stage["scene"]["brief"]
            + ("\nPrior scripted dialogue:\n" + previous if previous else ""))


def hook_cue(prompt: str, previous_fragment: str | None) -> tuple[str, str]:
    fragment = transfer.hooks._task_fragment(prompt)
    cue = (("Recent task: " + previous_fragment + "\n") if previous_fragment else "")
    return cue + "Current task: " + fragment, fragment


def actor_scene(stage: dict, messages: list[dict]) -> dict:
    return {"brief": "Scripted dialogue to date:\n" + dialogue_text(messages)
            + "\nCURRENT task: " + stage["scene"]["brief"],
            "commands": stage["scene"]["commands"]}


def actor_memory(contexts: list[dict]) -> tuple[str, list[str]]:
    rendered = "\n".join(c["context"] for c in contexts if c["context"])
    need(len(rendered.encode("utf-8")) <= MAX_ACTOR_MEMORY_BYTES,
         "prior delivered context exceeded actor memory window; refusing unfair seen suppression")
    return rendered, [card for c in contexts for card in c["delivered_ids"]]


def retention(selected: list[str], available: list[str], labels: dict) -> dict:
    present, chosen = set(available), set(selected)
    need(chosen <= present, "selected ID absent from native pool")
    keep, drop, allow = (set(labels.get(k, [])) & present for k in ("keep", "drop", "allow"))
    return {"available_keep": sorted(keep), "available_drop": sorted(drop),
            "available_allow": sorted(allow), "selected": sorted(chosen),
            "missed_keep": sorted(keep - chosen), "wrong_keep": sorted(chosen & drop),
            "exact_definite": keep <= chosen and not chosen & drop}


def native_pool(project: Path, db: Path, root: Path, mcp: Path, env: dict,
                cue: str) -> dict:
    """One real read per stage. Wider candidate admission is eval-local only."""
    root.mkdir()
    host = transfer.Host(mcp, root, "pool", ["--capability-profile", "read-only",
                 "--db", f"project={db}"], env)
    calls = []
    original_client = transfer.mcp_client.McpClient

    class Proxy(original_client):
        def call_tool(self, name, arguments):
            if name == "recall_context":
                need(arguments.get("depth") == 2 and arguments.get("observe") is True,
                     "collector recall contract changed")
                arguments = {**arguments, "k": 4, "max_nodes": MAX_POOL}
            value = super().call_tool(name, arguments)
            if name == "recall_context":
                need(value.get("schema") == "mneme.context.v5" and "probationary" not in value,
                     "native collector returned an obsolete context lifecycle")
                calls.append({"request": arguments,
                              "lane_ids": {lane: [c["id"] for c in value.get(lane, [])]
                                           for lane in ("primary", "episodes")},
                              "observation": value.get("observation")})
            return value

    try:
        service = root / "service.json"
        service.write_text(json.dumps({"binary": str(mcp), "project_db": str(db),
            "port": host.port, "state_dir": str(root / "service-state"),
            "working_directory": str(project)}), encoding="utf-8")
        started = time.monotonic()
        with patch.dict(os.environ, env), \
             patch.object(transfer.hook_recall, "McpClient", Proxy), \
             patch.object(transfer.hook_recall, "MAX_CARDS", MAX_POOL), \
             patch.object(transfer.hook_recall, "MAX_CARDS_BYTES", 12000):
            result = transfer.hook_recall._collect(service, cue, project, 1.5, observe=True)
        need(result.get("outcome") in ("ok", "empty") and len(result.get("cards", [])) <= MAX_POOL,
             "native pool collection failed")
        return {"result": result, "calls": calls,
                "elapsed_ms": int((time.monotonic() - started) * 1000)}
    finally:
        host.stop()


def observation_for(pool: dict, selected: list[dict]) -> dict | None:
    source = pool.get("observation")
    if not isinstance(source, dict) or not isinstance(source.get("cards"), list):
        return None
    ids = {c["id"] for c in selected}
    return {**source, "cards": [row for row in source["cards"]
                                if row.get("node_id") in ids]}


def reader_cards(pool: dict) -> list[dict]:
    observed = pool.get("observation")
    native = ({row["node_id"]: row for row in observed["cards"]}
              if isinstance(observed, dict) and isinstance(observed.get("cards"), list) else {})
    return [{**{k: c[k] for k in ("id", "summary", "source", "fingerprint") if k in c},
             **({"native": native[c["id"]]} if c["id"] in native else {})}
            for c in pool["cards"]]


def arm_result(arm: str, pool: dict, dialogue: list[dict], seen: list[tuple[str, str]],
               ledger: Path, live: bool, codex: Path, home: Path | None,
               workdir: Path, env: dict, model: str, effort: str,
               scope: str) -> tuple[dict, dict | None]:
    cards = pool["cards"]
    decision = None
    if arm == "off":
        selected = []
        outcome = "skipped"
    elif arm == "all":
        selected = cards[:MAX_READER_CARDS]
        outcome = pool["outcome"]
    else:
        decision = reader.select(dialogue, reader_cards(pool), ledger,
                                 live=live, codex=codex, home=home,
                                 workdir=workdir, env=env, model=model,
                                 effort=effort, scope=scope, seen=tuple(seen))
        ids = decision["selected_ids"]
        need(isinstance(ids, list) and len(ids) <= MAX_READER_CARDS
             and len(ids) == len(set(ids)) and set(ids) <= {c["id"] for c in cards},
             "reader selected an invalid native subset")
        selected = [c for c in cards if c["id"] in ids]
        outcome = pool["outcome"]  # An abstention is not a native empty read.
    return {"outcome": outcome, "cards": selected,
            "elapsed_ms": pool.get("elapsed_ms", 0),
            "observation": observation_for(pool, selected)}, decision


def hook_stage(project: Path, root: Path, arm: str, index: int, prompt: str,
               cue: str, pool_result: dict, dialogue: list[dict],
               seen: list[tuple[str, str]], ledger: Path, live: bool,
               codex: Path, home: Path | None, workdir: Path, env: dict,
               model: str, effort: str, scope: str) -> dict:
    config = {"project_root": project, "state_dir": root / "hook-state",
              "service_config": root / "unused-service.json", "recall_mode": "automatic",
              "memory_mode": "shadow", "memory_scope": "project"}
    session, turn = f"{root.name}-{arm}", f"stage-{index}"
    if index == 1:
        transfer.hooks.handle_event({"hook_event_name": "SessionStart", "cwd": str(project),
                                     "session_id": session, "source": "startup"}, config)
    event = {"hook_event_name": "UserPromptSubmit", "cwd": str(project),
             "session_id": session, "turn_id": turn, "prompt": prompt}
    selected_result, decision = arm_result(arm, pool_result, dialogue, seen,
        ledger, live, codex, home, workdir, env, model, effort, scope)
    def replay(_config, actual_cue):
        need(actual_cue == cue, "arm hook cue diverged from shared native read")
        return selected_result
    started = time.monotonic()
    with patch.object(transfer.hooks, "_recall_cards", replay):
        value = transfer.hooks.handle_event(event, config)
    elapsed = int((time.monotonic() - started) * 1000)
    context = value.get("hookSpecificOutput", {}).get("additionalContext", "")
    need(len(context.encode()) <= transfer.hooks.MAX_CONTEXT_BYTES,
         "actual hook context exceeded cap")
    state = json.loads((config["state_dir"] / (transfer.digest(session.encode()) + ".json")).read_text())
    turn_state = state["turns"][turn]
    delivered = turn_state["shadow"]["cards"]
    need(len(delivered) <= 2 and all(c["id"] in context for c in delivered),
         "hook delivery/state mismatch")
    delivered_ids = [c["id"] for c in delivered]
    selected_ids = [c["id"] for c in selected_result["cards"]]
    return {"context": context, "context_bytes": len(context.encode()),
            "delivered": delivered, "delivered_ids": delivered_ids,
            "selected_ids": selected_ids, "selector": decision,
            "selector_abstained": arm == "reader" and decision is not None
                                  and decision.get("reason") == "abstained",
            "selector_cached_empty": arm == "reader" and decision is not None
                                     and decision.get("reason") == "cache" and not selected_ids,
            "native_empty": pool_result["outcome"] == "empty",
            "recall_outcome": turn_state["recall"]["outcome"],
            "finalizer_suppressed_ids": [x for x in selected_ids if x not in delivered_ids],
            "cue": cue, "elapsed_ms": elapsed}


def actor_for(scene: dict, context: str, codex: Path, home: Path, env: dict,
              cache: dict, key: str, counters: dict) -> tuple[dict, str | None]:
    prompt = transfer.actor_prompt(scene, context).encode("utf-8")
    if prompt in cache:
        origin, result = cache[prompt]
        counters["actor_reuses"] += 1
        return result, origin
    need(counters["actor_calls"] < MAX_ACTOR_CALLS, "actor cap reached")
    result = transfer.actor_call(codex, home, Path(), env, scene, context)
    counters["actor_calls"] += 1
    cache[prompt] = (key, result)
    return result, None


def run_case(case: dict, lookup: dict, base: Path, mnemed: Path, mcp: Path,
             codex: Path, home: Path | None, ledger: Path, live: bool,
             run_actors: bool, model: str, effort: str, counters: dict):
    root = base / case["id"]
    root.mkdir()
    stable_reader_workdir = base / "reader-work"
    stable_reader_workdir.mkdir(exist_ok=True)
    env = transfer.fixture_env(root, mnemed)
    project, db, identifiers = transfer.fixture_case(case, lookup, "all", root, mnemed, env)
    arm_roots = {arm: root / arm for arm in ARMS}
    for folder in arm_roots.values():
        folder.mkdir()
    prior_messages: list[dict] = []
    previous_fragment = None
    histories = {arm: [] for arm in ARMS}
    seen = {arm: [] for arm in ARMS}
    actor_cache: dict = {}
    linked = False
    for index, stage in enumerate(case["stages"], 1):
        transfer.seed(db, project, mnemed, env, lookup, identifiers,
                      stage.get("seed_before_stage", []))
        edges = transfer.add_associations(case, "all", project, db, mnemed, env, identifiers) if not linked else []
        linked = True
        before = transfer.logical_cozo(db)
        prompt = hook_prompt(stage, prior_messages)
        all_messages = prior_messages + stage["dialogue_delta"]
        dialogue = dialogue_text(all_messages)
        cue, previous_fragment = hook_cue(prompt, previous_fragment)
        pool = native_pool(project, db, root / f"stage-{index}", mcp, env, cue)
        # The collector can open SQLite/WAL files, but logical Cozo KV must not change.
        need(transfer.logical_cozo(db) == before, "native read changed logical Cozo KV")
        pool_result = {**pool["result"], "elapsed_ms": pool["elapsed_ms"]}
        reverse_ids = {value: key for key, value in identifiers.items()}
        pool_fixture_ids = [reverse_ids.get(c["id"]) for c in pool_result["cards"]]
        for arm in ARMS:
            recalled = hook_stage(project, arm_roots[arm], arm, index, prompt,
                cue, pool_result, all_messages, seen[arm], ledger, live,
                codex, home, stable_reader_workdir, env, model, effort, f"{case['id']}:{arm}")
            need(transfer.logical_cozo(db) == before, "reader/hook changed logical Cozo KV")
            context_record = {"context": recalled["context"],
                              "delivered_ids": recalled["delivered_ids"]}
            histories[arm].append(context_record)
            pair_by_id = {c["id"]: c["fingerprint"] for c in pool_result["cards"]}
            seen[arm].extend((identifier, pair_by_id[identifier])
                             for identifier in recalled["delivered_ids"] if identifier in pair_by_id)
            memory, visible_ids = actor_memory(histories[arm])
            scene = actor_scene(stage, all_messages)
            actor = reused_from = grade = None
            if run_actors:
                actor, reused_from = actor_for(scene, memory, codex, home, env,
                    actor_cache, f"{case['id']}:stage-{index}:{arm}", counters)
                if actor.get("status") == "answered":
                    grade = transfer.score(actor["answer"]["actions"], stage["host"],
                                           stage["scene"]["commands"])
            need(transfer.logical_cozo(db) == before, "actor changed logical Cozo KV")
            selected_fixture = [reverse_ids.get(x) for x in recalled["selected_ids"]]
            delivered_fixture = [reverse_ids.get(x) for x in recalled["delivered_ids"]]
            visible_fixture = [reverse_ids.get(x) for x in visible_ids]
            yield {"case": case["id"], "split": case["split"], "stage": index,
                   "arm": arm, "seeded_ids": identifiers.copy(), "edges": edges,
                   "dialogue": dialogue, "hook_prompt": prompt,
                   "native_pool": {"candidate_ids": [c["id"] for c in pool_result["cards"]],
                                   "candidate_fixture_ids": pool_fixture_ids,
                                   "collector": pool["calls"], "elapsed_ms": pool["elapsed_ms"],
                                   "outcome": pool_result["outcome"]},
                   "recall": recalled, "selected_fixture_ids": selected_fixture,
                   "delivered_fixture_ids": delivered_fixture,
                   "retention": retention([x for x in selected_fixture if x],
                                          [x for x in pool_fixture_ids if x],
                                          stage["host"]["expected_retention"])
                                if arm == "reader" else None,
                   "actor_visible_retention": retention([x for x in visible_fixture if x],
                                                         [x for x in pool_fixture_ids + visible_fixture if x],
                                                         stage["host"]["expected_retention"]),
                   "actor_visible_memory_ids": visible_ids,
                   "actor_visible_fixture_ids": visible_fixture,
                   "actor_visible_memory_bytes": len(memory.encode()),
                   "actor": actor, "reused_from": reused_from, "grade": grade,
                   "persistent_kv_equal": True}
        prior_messages = all_messages


def totals(rows: list[dict]) -> dict:
    decisions = [row["recall"]["selector"] for row in rows if row["recall"].get("selector")]
    attempts = [decision for decision in decisions if decision.get("provider_attempt")]
    actors = [row["actor"] for row in rows if row.get("actor") and not row.get("reused_from")]
    return {"total_provider_attempts": len(attempts) + len(actors),
            "reader_provider_attempts": len(attempts),
            "reader_cache_hits": sum(bool(d.get("cache_hit")) for d in decisions),
            "reader_reason_counts": {reason: sum(str(d.get("reason")) == reason for d in decisions)
                                     for reason in sorted({str(d.get("reason")) for d in decisions})},
            "reader_usage": [d.get("usage") for d in attempts],
            "reader_elapsed_ms": sum(float(d.get("elapsed_ms") or 0) for d in attempts),
            "actor_provider_attempts": len(actors),
            "actor_reuses": sum(bool(row.get("reused_from")) for row in rows),
            "actor_usage": [a.get("usage") for a in actors],
            "actor_elapsed_ms": sum(float(a.get("elapsed_ms") or 0) for a in actors),
            "native_pool_elapsed_ms": sum(row["native_pool"]["elapsed_ms"]
                                          for row in rows if row["arm"] == "off")}


def preparation(args, cases_path: Path, cases: list[dict], mnemed: Path,
                mcp: Path, codex: Path) -> dict:
    sources = {name: transfer.artifact(ROOT / path) for name, path in {
        "runner": "tools/codex_reader_probe.py", "reader": "tools/codex_reader.py",
        "runner_tests": "tools/test_codex_reader_probe.py",
        "reader_tests": "tools/test_codex_reader.py",
        "transfer_baseline": "tools/reflexive_transfer_probe.py",
        "hook": "integrations/codex/hooks.py",
        "collector": "integrations/codex/hook_recall.py",
        "client": "integrations/codex/mcp_client.py"}.items()}
    protocol = ROOT / "tools/testdata/README.md"
    if protocol.is_file():
        sources["protocol"] = transfer.artifact(protocol)
    binaries = {}
    if args.preflight_development or args.run_readers or args.run_actors:
        binaries.update(mnemed=transfer.artifact(mnemed), mcp=transfer.artifact(mcp))
    if args.run_readers or args.run_actors:
        binaries["codex"] = transfer.artifact(codex)
    embedding = None
    if binaries:
        model_root = transfer.CACHE / "models--Xenova--bge-base-en-v1.5"
        revision = (model_root / "refs/main").read_text(encoding="utf-8").strip()
        blob = model_root / "snapshots" / revision / "onnx/model.onnx"
        need(blob.is_file(), "cached offline neural model unavailable")
        embedding = {"backend": "BGE 768 neural, offline", "revision": revision,
                     "blob": transfer.artifact(blob), "rerank": "disabled"}
    return {"schema": "mneme.codex-reader-probe.v1", "status": "prepared",
            "split": args.split, "case_file": transfer.artifact(cases_path),
            "case_ids": [c["id"] for c in cases],
            "source_commit": subprocess.run(["git", "rev-parse", "HEAD"], cwd=ROOT,
                text=True, capture_output=True, check=True).stdout.strip(),
            "sources": sources, "binaries": binaries, "embedding": embedding,
            "reader": {"live": args.run_readers, "model": args.model,
                       "effort": args.effort, "ledger": str(args.ledger) if args.run_readers else None},
            "actors": {"live": args.run_actors, "model": ACTOR_MODEL,
                       "effort": "low", "max_actual_calls": MAX_ACTOR_CALLS},
            "limits": {"native_pool": MAX_POOL, "reader_keep": MAX_READER_CARDS,
                       "dialogue_bytes": MAX_DIALOGUE_BYTES,
                       "actor_memory_bytes": MAX_ACTOR_MEMORY_BYTES},
            "caveats": ["evaluation-only; no production hook changes",
                        "same native read replayed to all arms within each stage",
                        "scripted dialogue only; earlier actor outputs not carried forward",
                        "no assessor, learning, or feedback"],
            "results": [], "actual_calls": {"actor_calls": 0, "actor_reuses": 0}}


def main() -> None:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--cases", type=Path, default=CASES)
    parser.add_argument("--mnemed", type=Path, default=transfer.BIN / "mnemed")
    parser.add_argument("--mcp", type=Path, default=transfer.BIN / "mneme-mcp")
    parser.add_argument("--codex", type=Path, default=transfer.CODEX)
    parser.add_argument("--split", choices=("development", "holdout"), default="development")
    parser.add_argument("--model", default="gpt-5.6-sol")
    parser.add_argument("--effort", default="low")
    parser.add_argument("--ledger", type=Path)
    parser.add_argument("--run-readers", action="store_true")
    parser.add_argument("--run-actors", action="store_true")
    parser.add_argument("--preflight-development", action="store_true")
    parser.add_argument("--output", required=True, type=Path)
    args = parser.parse_args()
    need(not args.run_readers or args.ledger is not None,
         "live reader requires explicit shared --ledger")
    need(not args.preflight_development or (args.split == "development"
         and not args.run_readers and not args.run_actors),
         "native preflight is development-only and provider-free")
    need(args.split != "holdout" or (not args.preflight_development
         and args.run_readers and args.run_actors) or (not args.run_readers and not args.run_actors),
         "holdout native evaluation requires both live arms")
    cases_path, mnemed, mcp, codex = (p.expanduser().resolve() for p in
                                      (args.cases, args.mnemed, args.mcp, args.codex))
    output = args.output.expanduser().resolve()
    need(not output.exists(), "refusing to overwrite existing output")
    need(cases_path.is_file(), "fixture missing")
    if args.preflight_development or args.run_readers or args.run_actors:
        need(mnemed.is_file() and mcp.is_file(), "frozen native binaries missing")
    if args.run_readers or args.run_actors:
        need(codex.is_file() and os.access(codex, os.X_OK), "Codex binary missing")
    if args.ledger is not None:
        args.ledger = args.ledger.expanduser().resolve()
    document = json.loads(cases_path.read_text(encoding="utf-8"))
    cases = inventory(document, args.split)
    receipt = preparation(args, cases_path, cases, mnemed, mcp, codex)
    output.parent.mkdir(parents=True, exist_ok=True)
    checkpoint(output, receipt)
    if args.preflight_development or args.run_readers or args.run_actors:
        receipt["status"] = "running"
        lookup = {card["id"]: card for card in document["cards"]}
        with tempfile.TemporaryDirectory(prefix="mneme-codex-reader-") as name:
            base = Path(name).resolve()
            home = None
            if args.run_readers or args.run_actors:
                auth = Path(os.environ.get("CODEX_HOME", Path.home() / ".codex")) / "auth.json"
                need(auth.is_file(), "Codex auth unavailable")
                home = base / "codex-home"
                home.mkdir(mode=0o700)
                (home / "auth.json").symlink_to(auth)
            for case in cases:
                for row in run_case(case, lookup, base, mnemed, mcp, codex, home,
                                    args.ledger or base / "offline-ledger.json",
                                    args.run_readers, args.run_actors,
                                    args.model, args.effort, receipt["actual_calls"]):
                    receipt["results"].append(row)
                    receipt["totals"] = totals(receipt["results"])
                    checkpoint(output, receipt)
        receipt["status"] = "completed" if args.run_readers or args.run_actors else "preflight_completed"
    receipt["totals"] = totals(receipt["results"])
    checkpoint(output, receipt)
    print(json.dumps({"status": receipt["status"], "output": str(output),
                      "rows": len(receipt["results"]), "totals": receipt["totals"]}))


if __name__ == "__main__":
    main()
