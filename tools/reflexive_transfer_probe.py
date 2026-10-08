#!/usr/bin/env python3
"""Disposable matched-task probe for the observation-only reflexive shadow loop.

Default is a no-provider preparation receipt. ``--run`` is the sole model-call
switch. No fixture, graph edge, actor answer, or assessor judgment is learned.
The holdout is never retrieved during preparation.
"""
from __future__ import annotations

import argparse
from contextlib import contextmanager
import hashlib
import json
import os
from pathlib import Path
import signal
import subprocess
import sys
import tempfile
import time
from unittest.mock import patch

sys.dont_write_bytecode = True
ROOT = Path(__file__).resolve().parents[1]
sys.path.insert(0, str(ROOT / "integrations" / "codex"))
from reflexive_shadow_smoke import cli, logical_cozo, sha  # noqa: E402
from memory_library_smoke import Host  # noqa: E402
import hooks  # noqa: E402
import hook_recall  # noqa: E402
import mcp_client  # noqa: E402
import shadow  # noqa: E402

CASES = ROOT / "tools/testdata/reflexive-transfer-v1/cases.json"
BIN = ROOT / "target/reflexive-shadow-v1/default/bin"
CODEX = Path.home() / ".local/bin/codex"
CACHE = Path.home() / ".local/share/mneme/.fastembed_cache"
MODEL = "gpt-5.6-sol"
ARMS = ("off", "graph_off", "graph_on")
MAX_ACTORS = 41  # 39 main-arm calls plus two prescribed shuffled-edge controls.
MAX_ASSESSORS = 28
ACTOR_TIMEOUT = 120
MAX_ACTOR_EVENTS = 1_000_000
MAX_ANSWER_BYTES = 4096


def need(ok: bool, why: str) -> None:
    if not ok:
        raise RuntimeError(why)


def digest(raw: bytes) -> str:
    return hashlib.sha256(raw).hexdigest()


def artifact(path: Path) -> dict:
    return {"path": str(path), "bytes": path.stat().st_size, "sha256": sha(path)}


def case_inventory(document: dict, split: str) -> list[dict]:
    need(document.get("version") == 1 and isinstance(document.get("cards"), list)
         and isinstance(document.get("cases"), list), "invalid case document")
    card_ids = {card["id"] for card in document["cards"]}
    need(len(card_ids) == len(document["cards"])
         and all(set(card) in ({"id", "text"}, {"id", "text", "kind"})
                 and isinstance(card["text"], str) and card["text"] for card in document["cards"]),
         "invalid card inventory")
    selected = []
    seen = set()
    for case in document["cases"]:
        need(case["id"] not in seen and case["split"] in ("development", "holdout")
             and isinstance(case["stages"], list) and case["stages"], "invalid case")
        seen.add(case["id"])
        for stage in case["stages"]:
            scene, host = stage["scene"], stage["host"]
            commands = scene["commands"]
            need(isinstance(scene["brief"], str) and isinstance(commands, list)
                 and len(commands) == len(set(commands)) and commands
                 and all(isinstance(c, str) and c for c in commands)
                 and all(isinstance(a, list) and all(x in commands for x in a)
                         for a in host["accepted_actions"])
                 and all(x in commands for x in host["damaging_actions"]), "invalid scene/gold")
            need(all(x in card_ids for x in stage.get("seed_before_stage", [])), "unknown stage seed")
        need(all(x in card_ids for x in case.get("seed_before_stage", [])), "unknown case seed")
        for pair in case.get("associations", {}).get("genuine", []) + case.get("associations", {}).get("shuffled", []):
            need(len(pair) == 2 and pair[0] in card_ids and pair[1] in card_ids,
                 "unknown association endpoint")
        if split == "all" or case["split"] == split:
            selected.append(case)
    need(selected, "no selected cases")
    return selected


def planned_counts(cases: list[dict]) -> dict:
    stages = sum(len(case["stages"]) for case in cases)
    controls = sum(bool(case.get("associations", {}).get("shuffled")) for case in cases)
    return {"cases": len(cases), "stages": stages,
            "actor_calls": stages * len(ARMS) + controls,
            "assessor_calls_max": stages * (len(ARMS) - 1) + controls,
            "shuffled_controls": controls}


def score(actions: list[str], host: dict, commands: list[str]) -> dict:
    need(isinstance(actions, list) and all(isinstance(a, str) for a in actions),
         "invalid actor actions")
    accepted = host["accepted_actions"]
    hazard = host.get("order_hazard")
    hazard_hit = False
    if hazard:
        need(set(hazard) == {"action", "requires_prior"}
             and hazard["action"] in commands and hazard["requires_prior"] in commands,
             "invalid order hazard")
        hazard_hit = any(action == hazard["action"] and hazard["requires_prior"] not in actions[:index]
                         for index, action in enumerate(actions))
    harm = any(a in host["damaging_actions"] for a in actions) or hazard_hit
    def lcs(a: list[str], b: list[str]) -> int:
        row = [0] * (len(b) + 1)
        for x in a:
            nxt = [0]
            for j, y in enumerate(b, 1):
                nxt.append(row[j-1] + 1 if x == y else max(nxt[-1], row[j]))
            row = nxt
        return row[-1]
    # Extra actions are detours; missing required actions are a correctness
    # failure, not invented extra actions.
    common = max((lcs(actions, gold) for gold in accepted), default=0)
    return {"success": actions in accepted, "harm": harm,
            "detours": max(0, len(actions) - common),
            "unknown_commands": [a for a in actions if a not in commands],
            "order_hazard_hit": hazard_hit}


def actor_prompt(scene: dict, context: str) -> str:
    # Scene is the only case material crossing into the actor. Hook output is
    # included verbatim, not reconstructed from fixture labels or gold.
    task = json.dumps(scene, ensure_ascii=False, separators=(",", ":"))
    return ("Choose console actions for this task. Use only listed commands. "
            "Return JSON with actions in order and a brief rationale. "
            "Do not use tools, search, or rate memory.\nTask: " + task
            + ("\n" + context if context else ""))


def actor_schema(commands: list[str]) -> dict:
    return {"type": "object", "additionalProperties": False,
            "required": ["actions", "rationale"],
            "properties": {"actions": {"type": "array", "maxItems": len(commands),
                                      "items": {"type": "string", "enum": commands}},
                           "rationale": {"type": "string", "maxLength": 500}}}


def parse_events(raw: bytes) -> dict:
    need(len(raw) <= MAX_ACTOR_EVENTS, "Codex event output exceeded 1 MiB")
    events = [json.loads(line) for line in raw.splitlines() if line.strip()]
    need(shadow._events_are_tool_free(raw), "actor emitted a tool or unexpected event")
    usage = [row.get("usage") for row in events if row.get("type") == "turn.completed"]
    return {"event_types": sorted({e["type"] for e in events}),
            "usage": usage[-1] if usage else None}


def _kill_group(process: subprocess.Popen) -> None:
    try:
        os.killpg(process.pid, signal.SIGKILL)
    except ProcessLookupError:
        pass
    process.communicate()


def actor_call(codex: Path, home: Path, cwd: Path, env: dict, scene: dict,
               context: str) -> dict:
    started = time.monotonic()
    with tempfile.TemporaryDirectory(prefix="mneme-actor-output-") as name:
        temp = Path(name)
        actor_cwd = temp / "empty-project"
        actor_cwd.mkdir()
        schema, output = temp / "schema.json", temp / "answer.json"
        schema.write_text(json.dumps(actor_schema(scene["commands"])), encoding="utf-8")
        argv = [str(codex), "exec", "--ignore-user-config", "--ignore-rules",
                "--ephemeral", "--skip-git-repo-check", "--sandbox", "read-only",
                "--disable", "hooks", "--disable", "memories", "--disable", "multi_agent",
                "--disable", "shell_tool", "-c", "project_doc_max_bytes=0",
                "-c", 'model_reasoning_effort="low"', "-m", MODEL, "--json",
                "--output-schema", str(schema), "-o", str(output), "-C", str(actor_cwd), "-"]
        process = subprocess.Popen(argv, cwd=actor_cwd, env={**env, "CODEX_HOME": str(home)},
                                   stdin=subprocess.PIPE, stdout=subprocess.PIPE,
                                   stderr=subprocess.PIPE, start_new_session=True)
        try:
            stdout, stderr = process.communicate(actor_prompt(scene, context).encode(),
                                                 timeout=ACTOR_TIMEOUT)
        except subprocess.TimeoutExpired:
            _kill_group(process)
            return {"status": "timeout", "elapsed_ms": int((time.monotonic() - started) * 1000)}
        result = {"status": "error", "elapsed_ms": int((time.monotonic() - started) * 1000),
                  "exit_code": process.returncode, "stderr_tail": stderr[-500:].decode(errors="replace")}
        try:
            result.update(parse_events(stdout))
            need(process.returncode == 0 and output.is_file() and not output.is_symlink(),
                 "actor did not produce output")
            raw = output.read_bytes()
            need(len(raw) <= MAX_ANSWER_BYTES, "actor answer exceeded 4 KiB")
            answer = json.loads(raw)
            need(isinstance(answer, dict) and set(answer) == {"actions", "rationale"}
                 and isinstance(answer["rationale"], str)
                 and len(answer["rationale"].encode()) <= 1000
                 and isinstance(answer["actions"], list)
                 and all(a in scene["commands"] for a in answer["actions"]),
                 "actor answer shape invalid")
            result.update(status="answered", answer=answer)
        except (ValueError, KeyError, TypeError, RuntimeError) as exc:
            result["failure"] = str(exc)
        return result


def fixture_env(root: Path, mnemed: Path) -> dict:
    env = os.environ.copy()
    env.pop("MNEME_DB", None)
    env.update({"MNEME_CLIENT_BINARY": str(mnemed), "MNEME_RERANK": "0",
                "HF_HUB_OFFLINE": "1", "TRANSFORMERS_OFFLINE": "1",
                "FASTEMBED_CACHE_DIR": str(CACHE),
                "GIT_CEILING_DIRECTORIES": str(root)})
    return env


def seed(db: Path, project: Path, mnemed: Path, env: dict, card_lookup: dict,
         identifiers: dict, wanted: list[str]) -> None:
    for key in wanted:
        if key in identifiers:
            continue
        card = card_lookup[key]
        need(card.get("kind", "semantic") == "semantic", "episode seed needs explicit fixture handling")
        payload = {"source": {"namespace": "transfer-probe", "key": key,
                              "reference": "fixture://reflexive-transfer/" + key},
                   "summary": card["text"], "body": card["text"]}
        identifiers[key] = cli(mnemed, project, env, "capture", "add", "--input", "-",
                               payload=payload, db=db)["id"]


def fixture_case(case: dict, cards: dict, arm: str, root: Path, mnemed: Path,
                 env: dict) -> tuple[Path, Path, dict]:
    project = root / "project"
    project.mkdir()
    (project / ".mneme").mkdir()
    (project / ".mneme/profile.json").write_text(
        '{"schema":"mneme.profile.v1","mode":"isolated"}\n', encoding="utf-8")
    db = project / ".mneme/codex-memory.db"
    cli(mnemed, project, env, "capture", "init", db=db)
    identifiers: dict[str, str] = {}
    seed(db, project, mnemed, env, cards, identifiers, case.get("seed_before_stage", []))
    need(db.is_file(), "single-graph capture init did not create the store")
    return project, db, identifiers


def add_associations(case: dict, arm: str, project: Path, db: Path, mnemed: Path,
                     env: dict, identifiers: dict) -> list[dict]:
    edges = case.get("associations", {}).get("shuffled" if arm == "shuffled" else "genuine", [])
    result = []
    for frm, to in edges:
        need(frm in identifiers and to in identifiers, "association endpoint not seeded")
        cli(mnemed, project, env, "link", "--from", identifiers[frm], "--to", identifiers[to],
            "--kind", "associative", "--weight", "0.7", db=db)
        result.append({"from": identifiers[frm], "to": identifiers[to], "kind": "associative"})
    return result


@contextmanager
def recall_override(service_config: Path, project: Path, depth: int, native_log: list[dict]):
    """Test-only seam: same hook handler/finalizer/renderer, native request depth changed.

    The production helper still decides lanes, two-card selection, get readback,
    fingerprint and observation; only this proxy rewrites the depth field.
    """
    original_client = mcp_client.McpClient

    class Proxy(original_client):
        def call_tool(self, name, arguments):
            if name == "recall_context":
                need(arguments.get("depth") == 2 and arguments.get("observe") is True,
                     "hook recall contract changed")
                arguments = {**arguments, "depth": depth}
            value = super().call_tool(name, arguments)
            if name == "recall_context":
                need(value.get("schema") == "mneme.context.v5" and "probationary" not in value,
                     "native collector returned an obsolete context lifecycle")
                native_log.append({"request": arguments, "observation": value.get("observation"),
                                   "lane_ids": {lane: [c["id"] for c in value.get(lane, [])]
                                                for lane in ("primary", "episodes")}})
            return value

    def collect(_config, cue):
        started = time.monotonic()
        with patch.object(hook_recall, "McpClient", Proxy):
            result = hook_recall._collect(service_config, cue, project, 1.5, observe=True)
        result["elapsed_ms"] = int((time.monotonic() - started) * 1000)
        return result

    with patch.object(hooks, "_recall_cards", collect):
        yield


def hook_context(project: Path, db: Path, root: Path, mnemed: Path, mcp: Path,
                 env: dict, arm: str, session: str, turn: str, scene: dict) -> dict:
    if arm == "off":
        return {"context": "", "delivered": [], "native": [], "recall_outcome": "off",
                "elapsed_ms": 0}
    host = Host(mcp, root, "read-" + turn,
                ["--capability-profile", "read-only", "--db", f"project={db}"], env)
    native: list[dict] = []
    try:
        service_config = root / ("service-" + turn + ".json")
        service_config.write_text(json.dumps({"binary": str(mcp), "project_db": str(db),
            "port": host.port, "state_dir": str(root / "service-state"),
            "working_directory": str(project)}), encoding="utf-8")
        config = {"project_root": project, "state_dir": root / "hook-state",
                  "service_config": service_config, "recall_mode": "automatic",
                  "memory_mode": "shadow", "memory_scope": "project"}
        event = {"hook_event_name": "UserPromptSubmit", "cwd": str(project),
                 "session_id": session, "turn_id": turn,
                 "prompt": "Decide the actions for this task: " + scene["brief"]}
        hooks.handle_event({"hook_event_name": "SessionStart", "cwd": str(project),
                            "session_id": session, "source": "startup"}, config)
        started = time.monotonic()
        with patch.dict(os.environ, env):
            with recall_override(service_config, project, 0 if arm == "graph_off" else 2, native):
                value = hooks.handle_event(event, config)
        elapsed = int((time.monotonic() - started) * 1000)
        context = value.get("hookSpecificOutput", {}).get("additionalContext", "")
        need(len(context.encode()) <= hooks.MAX_CONTEXT_BYTES, "hook exceeded actual context cap")
        state = json.loads((config["state_dir"] / (digest(session.encode()) + ".json")).read_text())
        turn_state = state["turns"][turn]
        delivered = turn_state["shadow"]["cards"]
        need(len(delivered) <= 2 and all(c["id"] in context for c in delivered),
             "hook delivery/state mismatch")
        return {"context": context, "context_bytes": len(context.encode()),
                "delivered": delivered, "native": native,
                "cue": turn_state["shadow"]["cue"],
                "recall_outcome": turn_state["recall"]["outcome"], "elapsed_ms": elapsed}
    finally:
        host.stop()


def assess(codex: Path, home: Path, cue: str, answer: dict, delivered: list[dict]) -> dict:
    if not delivered:
        return {"status": "no_delivery", "judgments": []}
    # shadow._run already applies its exact bounded rubric and tool-free
        # process contract. It does not expose Codex usage; record that gap.
    payload = {"codex": str(codex), "codex_sha256": sha(codex), "model": MODEL,
               "cue": cue[:shadow.MAX_CUE],
               "answer": json.dumps(answer, ensure_ascii=False)[:shadow.MAX_ANSWER],
               "cards": delivered}
    with patch.dict(os.environ, {"CODEX_HOME": str(home)}):
        result = shadow._run(payload)
    result["usage"] = None
    result["usage_note"] = "existing shadow._run does not expose Codex event usage"
    return result


def prepare_receipt(cases_path: Path, document: dict, cases: list[dict], split: str,
                    mnemed: Path, mcp: Path, codex: Path | None) -> dict:
    count = planned_counts(cases)
    need(count["actor_calls"] <= MAX_ACTORS and count["assessor_calls_max"] <= MAX_ASSESSORS,
         "planned model calls exceed hard cap")
    sources = {name: artifact(ROOT / path) for name, path in {
        "hooks": "integrations/codex/hooks.py", "collector": "integrations/codex/hook_recall.py",
        "shadow": "integrations/codex/shadow.py", "native_client": "integrations/codex/mcp_client.py",
        "harness": "tools/reflexive_transfer_probe.py"}.items()}
    preflight_path = ROOT / "target/reflexive-transfer-v1/neural-preflight.json"
    preflight = json.loads(preflight_path.read_text()) if preflight_path.is_file() else None
    model_root = CACHE / "models--Xenova--bge-base-en-v1.5"
    revision = (model_root / "refs/main").read_text(encoding="utf-8").strip()
    model_blob = model_root / "snapshots" / revision / "onnx/model.onnx"
    need(model_blob.is_file(), "offline neural model blob unavailable")
    return {"schema": "mneme.reflexive-transfer-probe.v1", "status": "prepared",
            "split": split, "case_file": artifact(cases_path),
            "source_commit": subprocess.run(["git", "rev-parse", "HEAD"], cwd=ROOT,
                text=True, capture_output=True, check=True).stdout.strip(),
            "sources": sources, "binaries": {"mnemed": artifact(mnemed), "mneme-mcp": artifact(mcp),
                         **({"codex": artifact(codex)} if codex else {})},
            "model": MODEL, "effort": "low", "counts": count,
            "limits": {"actor_timeout_seconds": ACTOR_TIMEOUT, "assessor_timeout_seconds": shadow.TIMEOUT,
                       "max_actor_calls": MAX_ACTORS, "max_assessor_calls": MAX_ASSESSORS,
                       "max_cards": 2, "max_hook_context_bytes": hooks.MAX_CONTEXT_BYTES,
                       "native_k": 2, "native_max_nodes": 4, "depths": {"off": None, "graph_off": 0, "graph_on": 2}},
            "case_ids": [case["id"] for case in cases],
            "embedding": {"backend": "default-feature neural binary; cached BGE 768 offline",
                          "rerank": "disabled", "cache": str(CACHE), "revision": revision,
                          "model_blob": artifact(model_blob), "preflight": preflight,
                          "preflight_file": artifact(preflight_path) if preflight else None,
                          "index_stamp": preflight.get("index_set") if preflight else None},
            "caveats": ["off has zero memory bytes; caps, not consumed token counts, are matched",
                        "native graph path is not causal proof", "assessor usage unavailable from existing shadow._run"]}


def run_cases(cases: list[dict], document: dict, mnemed: Path, mcp: Path,
              codex: Path, receipt: dict, output: Path) -> None:
    auth = Path(os.environ.get("CODEX_HOME", Path.home() / ".codex")) / "auth.json"
    need(auth.is_file(), "Codex auth unavailable")
    lookup = {card["id"]: card for card in document["cards"]}
    receipt["status"] = "running"
    receipt["results"] = []
    actor_calls = assessor_calls = 0
    with tempfile.TemporaryDirectory(prefix="mneme-transfer-probe-") as name:
        base = Path(name).resolve()
        home = base / "codex-home"
        home.mkdir(mode=0o700)
        (home / "auth.json").symlink_to(auth)
        for case in cases:
            for arm in (*ARMS, *(["shuffled"] if case.get("associations", {}).get("shuffled") else [])):
                root = base / f"{case['id']}-{arm}"
                root.mkdir()
                env = fixture_env(root, mnemed)
                project, db, identifiers = fixture_case(case, lookup, arm, root, mnemed, env)
                edges_added = False
                for index, stage in enumerate(case["stages"]):
                    seed(db, project, mnemed, env, lookup, identifiers,
                         stage.get("seed_before_stage", []))
                    if not edges_added:
                        edges = add_associations(case, arm, project, db, mnemed, env, identifiers)
                        edges_added = True
                    else:
                        edges = []
                    before = logical_cozo(db)
                    recalled = hook_context(project, db, root, mnemed, mcp, env, arm,
                                            f"{case['id']}-{arm}-stage-{index+1}",
                                            f"stage-{index+1}", stage["scene"])
                    need(logical_cozo(db) == before, "recall changed persistent store")
                    need(actor_calls < MAX_ACTORS, "actor hard cap reached")
                    actor_calls += 1
                    actor = actor_call(codex, home, project, env, stage["scene"], recalled["context"])
                    grade = (score(actor["answer"]["actions"], stage["host"], stage["scene"]["commands"])
                             if actor["status"] == "answered" else None)
                    if arm != "off" and recalled["delivered"] and actor["status"] == "answered":
                        need(assessor_calls < MAX_ASSESSORS, "assessor hard cap reached")
                        assessor_calls += 1
                        feedback = assess(codex, home, recalled["cue"],
                                          actor.get("answer", {}), recalled["delivered"])
                    else:
                        feedback = {"status": "actor_failed" if actor["status"] != "answered"
                                    else "no_delivery", "judgments": []}
                    need(logical_cozo(db) == before, "actor/assessor changed persistent store")
                    receipt["results"].append({"case": case["id"], "split": case["split"],
                        "stage": index + 1, "arm": arm, "seeded_ids": identifiers.copy(),
                        "edges": edges, "recall": recalled, "actor": actor, "grade": grade,
                        "feedback": feedback, "persistent_kv_equal": True})
                    receipt["actual_calls"] = {"actor": actor_calls, "assessor": assessor_calls}
                    output.write_text(json.dumps(receipt, indent=2, ensure_ascii=False) + "\n")
    receipt["status"] = "completed"


def preflight_development(cases: list[dict], document: dict, mnemed: Path,
                          mcp: Path, receipt: dict, output: Path) -> None:
    """Exercise native selection and real hook delivery without provider calls.

    The reserved holdout must not enter this path, even if a caller passes an
    accidentally mixed case list. The output is diagnostic, never tuned here.
    """
    need(all(case["split"] == "development" for case in cases),
         "native preflight is development-only")
    lookup = {card["id"]: card for card in document["cards"]}
    receipt["status"] = "preflight_running"
    receipt["preflight"] = []
    with tempfile.TemporaryDirectory(prefix="mneme-transfer-preflight-") as name:
        base = Path(name).resolve()
        for case in cases:
            for arm in (*ARMS, *(["shuffled"] if case.get("associations", {}).get("shuffled") else [])):
                root = base / f"{case['id']}-{arm}"
                root.mkdir()
                env = fixture_env(root, mnemed)
                project, db, identifiers = fixture_case(case, lookup, arm, root, mnemed, env)
                linked = False
                for index, stage in enumerate(case["stages"]):
                    seed(db, project, mnemed, env, lookup, identifiers,
                         stage.get("seed_before_stage", []))
                    if not linked:
                        edges = add_associations(case, arm, project, db, mnemed, env, identifiers)
                        linked = True
                    else:
                        edges = []
                    before = logical_cozo(db)
                    recalled = hook_context(project, db, root, mnemed, mcp, env, arm,
                                            f"{case['id']}-{arm}-stage-{index+1}",
                                            f"stage-{index+1}", stage["scene"])
                    need(logical_cozo(db) == before, "preflight recall changed persistent store")
                    receipt["preflight"].append({"case": case["id"], "stage": index + 1,
                        "arm": arm, "seeded_ids": identifiers.copy(), "edges": edges,
                        "recall": recalled, "persistent_kv_equal": True})
                    output.write_text(json.dumps(receipt, indent=2, ensure_ascii=False) + "\n")
    receipt["status"] = "preflight_completed"


def main() -> None:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--cases", type=Path, default=CASES)
    parser.add_argument("--mnemed", type=Path, default=BIN / "mnemed")
    parser.add_argument("--mcp", type=Path, default=BIN / "mneme-mcp")
    parser.add_argument("--codex", type=Path, default=CODEX)
    parser.add_argument("--split", choices=("development", "holdout", "all"), default="development")
    parser.add_argument("--output", required=True, type=Path)
    parser.add_argument("--run", action="store_true", help="explicitly authorize bounded provider calls")
    parser.add_argument("--preflight-development", action="store_true",
                        help="no-provider native retrieval/delivery diagnostic; refuses holdout")
    args = parser.parse_args()
    need(not (args.run and args.preflight_development), "choose --run or --preflight-development")
    need(not args.preflight_development or args.split == "development",
         "native preflight may inspect development only")
    cases_path, mnemed, mcp, codex = (p.resolve() for p in
                                      (args.cases, args.mnemed, args.mcp, args.codex))
    for path in (cases_path, mnemed, mcp):
        need(path.is_file(), f"required artifact missing: {path}")
    need(not args.run or (codex.is_file() and os.access(codex, os.X_OK)),
         "--run needs an executable Codex binary")
    document = json.loads(cases_path.read_text(encoding="utf-8"))
    cases = case_inventory(document, args.split)
    output = args.output.resolve()
    output.parent.mkdir(parents=True, exist_ok=True)
    receipt = prepare_receipt(cases_path, document, cases, args.split, mnemed, mcp,
                              codex if args.run else None)
    if args.run:
        run_cases(cases, document, mnemed, mcp, codex, receipt, output)
    elif args.preflight_development:
        preflight_development(cases, document, mnemed, mcp, receipt, output)
    output.write_text(json.dumps(receipt, indent=2, ensure_ascii=False) + "\n")
    print(json.dumps({"status": receipt["status"], "output": str(output),
                      "counts": receipt["counts"]}))


if __name__ == "__main__":
    main()
