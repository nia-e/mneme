#!/usr/bin/env python3
"""Frozen six-stage, retrieval-bypassed probe of the production ReaderRuntime.

Default prepares a no-provider receipt. --run consumes that exact receipt once.
No actor, native retrieval, store, hooks, or live configuration is involved.
"""
from __future__ import annotations

import argparse
import hashlib
import json
import os
from pathlib import Path
import statistics
import sys
import tempfile

sys.dont_write_bytecode = True
ROOT = Path(__file__).resolve().parents[1]
sys.path.insert(0, str(ROOT / "integrations" / "codex"))
from reader_contract import prepare  # noqa: E402
from reader_runtime import ReaderRuntime, MODEL, USAGE_FIELDS  # noqa: E402
from reader_worker import _fragment, _short, MAX_PROMPT  # noqa: E402

FIXTURE = ROOT / "tools/testdata/async-reader-v1/cases.json"
PROTOCOL = ROOT / "target/async-reader-v1/runtime-probe-protocol.json"
RESULT = ROOT / "target/async-reader-v1/runtime-probe-results.json"
BIN = (Path.home() / ".local/bin/codex").resolve()
SCHEMA = "mneme.async-reader-runtime-probe.v1"
EXPECTED_STAGES = 6


def digest(path: Path) -> str:
    return hashlib.sha256(path.read_bytes()).hexdigest()


def source_paths() -> list[Path]:
    return [FIXTURE, Path(__file__).resolve(),
            ROOT / "tools/test_async_reader_runtime_probe.py",
            ROOT / "integrations/codex/reader_runtime.py",
            ROOT / "integrations/codex/reader_contract.py",
            ROOT / "integrations/codex/reader_worker.py", PROTOCOL, BIN]


def manifest() -> dict[str, str]:
    return {str(path): digest(path) for path in source_paths()}


def write(path: Path, value: dict) -> None:
    path.parent.mkdir(parents=True, exist_ok=True)
    temporary = path.with_suffix(path.suffix + ".tmp")
    temporary.write_text(json.dumps(value, indent=2, ensure_ascii=False) + "\n", encoding="utf-8")
    temporary.replace(path)


def need(condition: bool, explanation: str) -> None:
    if not condition:
        raise ValueError(explanation)


def inventory() -> list[dict]:
    document = json.loads(FIXTURE.read_text(encoding="utf-8"))
    need(document.get("version") == 1 and isinstance(document.get("cards"), list)
         and isinstance(document.get("stages"), list), "invalid fixture shape")
    cards = document["cards"]
    lookup = {card["id"]: card for card in cards}
    need(len(lookup) == len(cards) and len(document["stages"]) == EXPECTED_STAGES,
         "duplicate card or wrong stage count")
    stages = []
    seen_stages = set()
    prior = ""
    for stage in document["stages"]:
        stage_id, prompt, ids, host = (stage[key] for key in
                                       ("id", "current_prompt", "available_ids", "host"))
        need(isinstance(stage_id, str) and stage_id not in seen_stages
             and isinstance(prompt, str) and prompt.strip()
             and isinstance(ids, list) and 0 < len(ids) <= 6
             and len(ids) == len(set(ids)) and all(key in lookup for key in ids),
             "invalid stage or card pool")
        seen_stages.add(stage_id)
        need(isinstance(host, dict) and all(isinstance(host.get(key), list)
                                          for key in ("keep", "drop", "allow")),
             "invalid host rubric")
        keep, drop, allow = (set(host[key]) for key in ("keep", "drop", "allow"))
        need(not (keep & drop or keep & allow or drop & allow)
             and keep | drop | allow == set(ids), "host labels do not partition pool")
        cue = ("Recent task: " + prior + "\n" if prior else "") + "Current task: " + _short(prompt, MAX_PROMPT)
        pool = [lookup[key] for key in ids]
        prepared, _ = prepare([{"role": "user", "text": cue}], pool)
        need(prepared is not None, "reader preflight rejected stage " + stage_id)
        stages.append({"id": stage_id, "cue": cue, "cards": pool, "host": host})
        prior = _fragment(prompt)
    return stages


def grade(ids: list[str], host: dict) -> dict:
    keep, drop = set(host["keep"]), set(host["drop"])
    chosen = set(ids)
    return {"acceptable": keep <= chosen and not drop & chosen,
            "missed_keep": sorted(keep - chosen),
            "wrong_keep": sorted(drop & chosen)}


def summary(rows: list[dict]) -> dict:
    usages = [row["reader"]["usage"] for row in rows if row["reader"].get("provider_attempt")]
    keys = list(USAGE_FIELDS.values()) + ["uncached_input_tokens"]
    latencies = [row["reader"]["elapsed_ms"] for row in rows]
    return {"stages": len(rows), "provider_attempts": sum(bool(r["reader"]["provider_attempt"]) for r in rows),
            "valid_grades": sum(r["grade"] is not None for r in rows),
            "acceptable": sum(bool(r["grade"] and r["grade"]["acceptable"]) for r in rows),
            "required_kept": sum(len(set(r["host_keep"]) & set(r["reader"]["selected_ids"])) for r in rows),
            "required_total": sum(len(r["host_keep"]) for r in rows),
            "wrong_kept": sum(len(r["grade"]["wrong_keep"]) for r in rows if r["grade"]),
            "usage": {key: sum((usage or {}).get(key, 0) for usage in usages) for key in keys},
            "missing_usage": {key: sum(usage is None or usage.get(key) is None for usage in usages) for key in keys},
            "latency_ms_total": sum(latencies),
            "latency_ms_median": statistics.median(latencies) if latencies else None}


def prepare_receipt(stages: list[dict]) -> None:
    need(not RESULT.exists() and not PROTOCOL.exists(), "probe receipt/protocol already exists")
    protocol = {"schema": SCHEMA, "model": MODEL, "effort": "low", "max_provider_attempts": 6,
                "stages": [stage["id"] for stage in stages], "reader": "production ReaderRuntime.select",
                "input": "worker cue: prior 360-byte normalized fragment plus current prompt; frozen supplied card pool",
                "boundary": "native retrieval bypassed; no actor, store, hook, config mutation, retry, or model escalation",
                "stop": "first invalid output, failure, unknown usage, or changed source hash"}
    write(PROTOCOL, protocol)
    frozen = manifest()
    write(RESULT, {"schema": SCHEMA, "status": "prepared", "manifest": frozen,
                   "attempts": 0, "rows": [], "summary": summary([])})


def run(stages: list[dict]) -> None:
    receipt = json.loads(RESULT.read_text(encoding="utf-8"))
    need(receipt.get("schema") == SCHEMA and receipt.get("status") == "prepared"
         and receipt.get("attempts") == 0 and receipt.get("rows") == []
         and receipt.get("manifest") == manifest(), "not an untouched frozen prepared receipt")
    auth = Path(os.environ.get("CODEX_HOME", str(Path.home() / ".codex"))) / "auth.json"
    need(BIN.is_file() and os.access(BIN, os.X_OK) and auth.is_file(), "missing Codex executable/auth")
    config = {"reader_codex": str(BIN), "reader_codex_sha256": receipt["manifest"][str(BIN)],
              "reader_model": MODEL, "reader_auth": str(auth.resolve())}
    receipt["status"] = "running"
    write(RESULT, receipt)
    with tempfile.TemporaryDirectory(prefix="mneme-async-reader-runtime-") as temp:
        with ReaderRuntime(config, Path(temp) / "scratch") as runtime:
            for stage in stages:
                if manifest() != receipt["manifest"]:
                    receipt["status"] = "stopped_source_changed"
                    write(RESULT, receipt)
                    return
                reader = runtime.select([{"role": "user", "text": stage["cue"]}], stage["cards"])
                receipt["attempts"] += int(bool(reader["provider_attempt"]))
                valid = (reader["provider_attempt"] and reader["reason"] in ("selected", "abstained")
                         and reader["usage"] is not None and manifest() == receipt["manifest"])
                row = {"stage": stage["id"], "available_ids": [c["id"] for c in stage["cards"]],
                       "host_keep": stage["host"]["keep"], "reader": reader,
                       "grade": grade(reader["selected_ids"], stage["host"]) if valid else None}
                receipt["rows"].append(row)
                receipt["summary"] = summary(receipt["rows"])
                receipt["status"] = "running" if valid else "stopped"
                if manifest() != receipt["manifest"]:
                    receipt["status"] = "stopped_source_changed"
                write(RESULT, receipt)
                if receipt["status"] != "running":
                    return
    receipt["status"] = "completed"
    write(RESULT, receipt)


def main() -> None:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--run", action="store_true", help="consume prepared receipt; up to six actual reader calls")
    args = parser.parse_args()
    stages = inventory()
    if args.run:
        run(stages)
    else:
        prepare_receipt(stages)
    receipt = json.loads(RESULT.read_text(encoding="utf-8"))
    print(json.dumps({"status": receipt["status"], "attempts": receipt["attempts"],
                      "summary": receipt["summary"], "path": str(RESULT)}, indent=2))


if __name__ == "__main__":
    main()
