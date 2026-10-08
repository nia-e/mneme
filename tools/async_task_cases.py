#!/usr/bin/env python3
"""Provider-free task fixtures for the next async-memory comparison.

Validate, freeze a preparation receipt, or project one development scene. This
module never starts an actor, queries memory, installs hooks, or opens a store.
The preparation receipt freezes fixtures, not a yet-unbuilt end-to-end runner.
"""
from __future__ import annotations

import argparse
from copy import deepcopy
import hashlib
import json
import math
from pathlib import Path, PurePosixPath
import re

ROOT = Path(__file__).resolve().parents[1]
CASES = ROOT / "tools/testdata/async-task-v1/cases.json"
PROTOCOL = ROOT / "tools/testdata/README.md"
MAX_FIXTURE_BYTES = 256 * 1024


def need(value, message):
    if not value:
        raise ValueError(message)


def fields(value, expected, where):
    need(isinstance(value, dict) and set(value) == set(expected.split()),
         "invalid fields: " + where)


def text(value, where, limit=8192):
    need(isinstance(value, str) and value.strip() and len(value.encode()) <= limit,
         "invalid text: " + where)


def strings(value, where, *, empty=False):
    need(isinstance(value, list) and (empty or bool(value))
         and all(isinstance(x, str) and x for x in value)
         and len(value) == len(set(value)), "invalid string list: " + where)


def validate(document):
    fields(document, "version cards cases", "document")
    need(type(document["version"]) is int and document["version"] == 1, "unsupported version")
    cards, cases = document["cards"], document["cases"]
    need(isinstance(cards, list) and 1 <= len(cards) <= 120, "invalid card count")
    need(isinstance(cases, list) and 1 <= len(cases) <= 20, "invalid case count")
    keys = set()
    for card in cards:
        fields(card, "key summary body kind thread", "card")
        text(card["key"], "card key", 80)
        need(card["key"] not in keys, "duplicate card key")
        keys.add(card["key"])
        text(card["summary"], "card summary", 700)
        text(card["body"], "card body", 4096)
        need(card["kind"] in ("semantic", "episode"), "invalid card kind")
        if card["thread"] is not None:
            need(card["kind"] == "episode", "only episodes have thread metadata")
            text(card["thread"], "episode thread", 120)
    ids = set()
    for case in cases:
        fields(case, "id split family scene memory_keys edges host timing", "case")
        text(case["id"], "case id", 80)
        need(case["id"] not in ids, "duplicate case id")
        ids.add(case["id"])
        need(case["split"] in ("development", "holdout"), "invalid split")
        text(case["family"], "case family", 120)
        scene, host, timing = case["scene"], case["host"], case["timing"]
        fields(scene, "prompt files actions max_actions tool_policy", "scene")
        text(scene["prompt"], "scene prompt")
        need(isinstance(scene["files"], dict) and len(scene["files"]) <= 8, "invalid scene files")
        for name, contents in scene["files"].items():
            text(name, "scene filename", 160)
            path = PurePosixPath(name)
            need(not path.is_absolute() and ".." not in path.parts and str(path) == name
                 and "\\" not in name and name != ".", "scene path must be normalized and relative")
            text(contents, "scene file", 8192)
        strings(scene["actions"], "scene actions")
        need(len(scene["actions"]) <= 12 and all(len(a.encode()) <= 160 for a in scene["actions"]),
             "too many/long actions")
        need(type(scene["max_actions"]) is int and 1 <= scene["max_actions"] <= 8,
             "invalid max_actions")
        need(scene["tool_policy"] in ("allowed", "none"), "invalid tool policy")
        strings(case["memory_keys"], "memory keys")
        need(len(case["memory_keys"]) <= 12 and set(case["memory_keys"]) <= keys,
             "unknown or excessive memory inventory")
        fields(host, "accepted_plans deferred_plans forbidden_actions required_memory irrelevant_memory "
               "memory_expectation information_gap decision_rationale", "host")
        need(isinstance(host["accepted_plans"], list) and host["accepted_plans"], "no accepted plans")
        for plan in host["accepted_plans"]:
            need(isinstance(plan, list) and len(plan) <= scene["max_actions"]
                 and all(isinstance(a, str) and a in scene["actions"] for a in plan),
                 "invalid accepted plan")
        need(len({tuple(p) for p in host["accepted_plans"]}) == len(host["accepted_plans"]),
             "duplicate accepted plan")
        need(isinstance(host["deferred_plans"], list)
             and all(isinstance(p, list) and p in host["accepted_plans"] for p in host["deferred_plans"]),
             "deferred plan must be accepted")
        need(len({tuple(p) for p in host["deferred_plans"]}) == len(host["deferred_plans"]),
             "duplicate deferred plan")
        strings(host["forbidden_actions"], "forbidden actions", empty=True)
        need(set(host["forbidden_actions"]) <= set(scene["actions"]), "unknown forbidden action")
        need(not any(set(p) & set(host["forbidden_actions"]) for p in host["accepted_plans"]),
             "accepted plan includes forbidden action")
        for name in ("required_memory", "irrelevant_memory"):
            strings(host[name], name, empty=True)
            need(set(host[name]) <= set(case["memory_keys"]), "unseeded " + name)
        need(len(host["required_memory"]) <= 2, "required memories exceed delivery budget")
        need(not set(host["required_memory"]) & set(host["irrelevant_memory"]),
             "memory cannot be required and irrelevant")
        need(host["memory_expectation"] in ("needed", "unnecessary", "ambiguous"),
             "invalid memory expectation")
        if host["memory_expectation"] == "unnecessary":
            need(not host["required_memory"], "unnecessary case requires memory")
        if host["memory_expectation"] == "needed":
            need(bool(host["required_memory"]), "needed case lacks target memory")
        text(host["information_gap"], "information gap")
        text(host["decision_rationale"], "decision rationale")
        need(isinstance(case["edges"], list) and len(case["edges"]) <= 16, "invalid edge list")
        seen_edges = set()
        for edge in case["edges"]:
            fields(edge, "from to kind weight", "edge")
            need(isinstance(edge["from"], str) and isinstance(edge["to"], str)
                 and edge["from"] in case["memory_keys"] and edge["to"] in case["memory_keys"]
                 and edge["from"] != edge["to"], "invalid edge endpoints")
            need(edge["kind"] in ("associative", "derived_from", "transition"), "invalid edge kind")
            need(type(edge["weight"]) in (int, float) and math.isfinite(edge["weight"])
                 and 0 < edge["weight"] <= 1, "invalid edge weight")
            key = (edge["from"], edge["to"], edge["kind"])
            need(key not in seen_edges, "duplicate edge")
            seen_edges.add(key)
        fields(timing, "kind decision_event notes", "timing")
        need(timing["kind"] in ("organic_tools", "no_tool")
             and timing["decision_event"] == "first_submission", "invalid timing contract")
        need((scene["tool_policy"] == "none") == (timing["kind"] == "no_tool"),
             "tool/timing mismatch")
        text(timing["notes"], "timing notes")
        if scene["tool_policy"] == "none":
            need(not scene["files"], "no-tool scene cannot depend on unreadable files")
    return document


def load(path=CASES):
    raw = Path(path).read_bytes()
    need(len(raw) <= MAX_FIXTURE_BYTES, "fixture too large")
    return validate(json.loads(raw))


def actor_scene(case):
    """Prompt projection: file paths, never file contents or host labels.

    Materialize workspace_files separately so inspection has real tool boundaries.
    """
    scene = case["scene"]
    return {"prompt": scene["prompt"], "file_paths": sorted(scene["files"]),
            "actions": deepcopy(scene["actions"]), "max_actions": scene["max_actions"],
            "tool_policy": scene["tool_policy"]}


def workspace_files(case):
    return deepcopy(case["scene"]["files"])


def grade(case, actions):
    """Score a submitted plan, not executed effects or causal memory usefulness."""
    scene, host = case["scene"], case["host"]
    valid = (isinstance(actions, list) and len(actions) <= scene["max_actions"]
             and all(isinstance(a, str) and a in scene["actions"] for a in actions))
    supplied = {a for a in actions if isinstance(a, str)} if isinstance(actions, list) else set()
    success = valid and actions in host["accepted_plans"]
    resolution = ("invalid" if not valid else "incorrect" if not success
                  else "deferred" if actions in host["deferred_plans"] else "ready")
    return {"valid": valid, "success": success, "resolution": resolution,
            "forbidden_action": bool(supplied & set(host["forbidden_actions"]))}


def first_submission(case, events):
    """Find the first task-plan publication, not the last checkpoint answer.

    Wrong plans count. Only non-plan messages are skipped. No event time or memory
    visibility is inferred from this ordered, potentially buffered event list.
    """
    need(isinstance(events, list) and len(events) <= 1024, "invalid event count")
    for index, event in enumerate(events):
        if not isinstance(event, dict) or event.get("type") != "item.completed":
            continue
        item = event.get("item")
        if not isinstance(item, dict) or item.get("type") != "agent_message":
            continue
        raw = item.get("text")
        if not isinstance(raw, str):
            continue
        def invalid(reason):
            return {"event_index": index, "message_sha256": hashlib.sha256(raw.encode()).hexdigest(),
                    "answer": None, "grade": {"valid": False, "success": False,
                                               "forbidden_action": None, "resolution": "invalid"},
                    "reason": reason, "timing": "event_order_only", "memory_visibility": "unknown"}
        # An oversized message is a protocol failure, even if we cannot identify
        # its decision. Never skip it and award a later corrected plan success.
        if len(raw.encode()) > 4096:
            return invalid("oversized_agent_message")
        def unique_object(pairs):
            result = {}
            for key, value in pairs:
                if key in result:
                    raise ValueError("duplicate JSON key")
                result[key] = value
            return result
        try:
            answer = json.loads(raw, object_pairs_hook=unique_object)
        except ValueError:
            if re.search(r'''["']actions["']\s*:''', raw):
                return invalid("malformed_task_plan")
            continue
        if not isinstance(answer, dict) or "actions" not in answer:
            if re.search(r'''["']actions["']\s*:''', raw):
                return invalid("invalid_task_plan_wrapper")
            continue
        verdict = grade(case, answer["actions"])
        if set(answer) != {"actions", "rationale"} or not isinstance(answer.get("rationale"), str):
            verdict.update(valid=False, success=False, resolution="invalid")
        return {"event_index": index, "message_sha256": hashlib.sha256(raw.encode()).hexdigest(),
                "answer": answer, "grade": verdict,
                "timing": "event_order_only", "memory_visibility": "unknown"}
    return None


def inventory(document):
    return {"cases": len(document["cases"]), "cards": len(document["cards"]),
            "splits": {split: sum(c["split"] == split for c in document["cases"])
                       for split in ("development", "holdout")},
            "no_tool_cases": sum(c["scene"]["tool_policy"] == "none" for c in document["cases"]),
            "base_actor_calls_planned": 3 * len(document["cases"]),
            "provider_calls_made": 0, "native_retrieval_performed": False}


def prepare(path, fixture=CASES):
    document = load(fixture)
    paths = [Path(fixture), Path(__file__).resolve(),
             ROOT / "tools/test_async_task_cases.py", PROTOCOL]
    receipt = {"schema": "mneme.async-task-fixtures.v1", "status": "prepared_not_run",
               "inventory": inventory(document),
               "manifest": {str(p.relative_to(ROOT)) if p.is_relative_to(ROOT) else str(p):
                            hashlib.sha256(p.read_bytes()).hexdigest() for p in paths},
               "boundary": "Fixture/scorer freeze only. No provider/native preflight, including holdout. "
                           "The future timing runner must be reviewed and frozen separately."}
    path = Path(path)
    need(not path.exists(), "refusing to overwrite preparation receipt")
    path.parent.mkdir(parents=True, exist_ok=True)
    with path.open("x", encoding="utf-8") as stream:
        stream.write(json.dumps(receipt, indent=2) + "\n")
    return receipt


def main(argv=None):
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--cases", type=Path, default=CASES)
    mode = parser.add_mutually_exclusive_group(required=True)
    mode.add_argument("--validate", action="store_true")
    mode.add_argument("--prepare", type=Path, metavar="NEW_RECEIPT")
    mode.add_argument("--actor-case", metavar="DEVELOPMENT_CASE_ID")
    args = parser.parse_args(argv)
    try:
        if args.prepare:
            output = prepare(args.prepare, args.cases)
        else:
            document = load(args.cases)
            if args.actor_case:
                case = next((c for c in document["cases"] if c["id"] == args.actor_case), None)
                need(case is not None, "unknown case")
                need(case["split"] == "development", "holdout projection reserved for the frozen run")
                output = actor_scene(case)
            else:
                output = inventory(document)
        print(json.dumps(output, indent=2))
    except (OSError, ValueError) as exc:
        parser.error(str(exc))


if __name__ == "__main__":
    main()
