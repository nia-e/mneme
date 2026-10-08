#!/usr/bin/env python3
"""One frozen, evaluation-only Astra follow-up to the existing reader comparison."""
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
sys.path.insert(0, str(ROOT / "tools"))
import codex_reader as reader
import codex_reader_compare as prior
from codex_reader_probe import retention

MODEL = "gpt-6-astra"
EFFORT = "low"
OUTPUT = ROOT / "target/codex-reader-astra-v1"
PROTOCOL = ROOT / "tools/testdata/README.md"
SCHEMA = "mneme.reader-astra-followup.v1"


def digest(path: Path) -> str:
    return hashlib.sha256(path.read_bytes()).hexdigest()


def sources(codex: Path) -> list[Path]:
    return [prior.CASES, Path(__file__).resolve(), ROOT / "tools/test_codex_reader_astra_probe.py",
            ROOT / "tools/codex_reader.py", ROOT / "tools/codex_reader_compare.py",
            ROOT / "tools/codex_reader_probe.py", PROTOCOL, codex]


def write(path: Path, value: dict) -> None:
    temp = path.with_suffix(".tmp")
    temp.write_text(json.dumps(value, indent=2, ensure_ascii=False) + "\n", encoding="utf-8")
    temp.replace(path)


def summarize(rows: list[dict]) -> dict:
    valid = [r for r in rows if r["grade"] is not None]
    usage = [r["result"].get("usage") for r in rows if r["result"].get("provider_attempt")]
    keys = (*reader.USAGE_KEYS, "uncached_input_tokens")
    times = [r["result"]["elapsed_ms"] for r in valid]
    return {
        "cases": len(rows), "valid": len(valid),
        "acceptable": sum(r["grade"]["exact_definite"] for r in valid),
        "missed_keep": sum(len(r["grade"]["missed_keep"]) for r in valid),
        "wrong_keep": sum(len(r["grade"]["wrong_keep"]) for r in valid),
        "correct_abstention": sum(r["kind"] == "abstain" and not r["result"]["selected_ids"] for r in valid),
        "by_kind": {kind: {"cases": sum(r["kind"] == kind for r in valid),
                            "acceptable": sum(r["kind"] == kind and r["grade"]["exact_definite"] for r in valid)}
                    for kind in ("useful", "abstain", "hard")},
        "usage": {key: sum((u or {}).get(key) or 0 for u in usage) for key in keys},
        "missing_usage": {key: sum(u is None or u.get(key) is None for u in usage) for key in keys},
        "latency_ms_median": statistics.median(times) if times else None,
        "latency_ms_total": sum(r["result"]["elapsed_ms"] for r in rows),
    }


def main() -> None:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--run", action="store_true", help="consume an existing prepared receipt once")
    parser.add_argument("--output", type=Path, default=OUTPUT)
    args = parser.parse_args()
    cases = prior.inventory(json.loads(prior.CASES.read_text(encoding="utf-8")))
    out = args.output.resolve()
    result_path = out / "results.json"
    codex = (Path.home() / ".local/bin/codex").resolve()
    frozen = {str(path): digest(path) for path in sources(codex)}
    if not args.run:
        out.mkdir(parents=True, exist_ok=False)
        receipt = {"schema": SCHEMA, "status": "prepared", "model": MODEL,
                   "effort": EFFORT, "max_attempts": 12, "files": frozen,
                   "attempts": 0, "results": [], "summary": summarize([])}
        write(result_path, receipt)
        print(json.dumps({"status": "prepared", "output": str(result_path)}))
        return
    receipt = json.loads(result_path.read_text(encoding="utf-8"))
    if (receipt.get("schema") != SCHEMA or receipt.get("status") != "prepared"
            or receipt.get("results") != [] or receipt.get("attempts") != 0
            or receipt.get("files") != frozen or (out / "ledger.json").exists()):
        raise ValueError("run requires untouched prepared receipt and frozen sources")
    auth = Path(os.environ.get("CODEX_HOME", str(Path.home() / ".codex"))) / "auth.json"
    if not codex.is_file() or not os.access(codex, os.X_OK) or not auth.is_file():
        raise ValueError("missing Codex executable/auth")
    receipt["status"] = "running"
    write(result_path, receipt)
    with tempfile.TemporaryDirectory(prefix="mneme-reader-astra-") as tmp:
        base = Path(tmp).resolve()
        home, cwd = base / "home", base / "work"
        home.mkdir(mode=0o700)
        cwd.mkdir()
        (home / "auth.json").symlink_to(auth)
        for case in cases:
            if receipt["attempts"] >= 12:
                break
            cards = reader._cards(case["cards"])
            try:
                result = reader.select(case["dialogue"], cards, out / "ledger.json",
                    live=True, codex=codex, home=home, workdir=cwd, env=os.environ.copy(),
                    model=MODEL, effort=EFFORT, scope=f"astra-followup:{case['id']}")
            except Exception as exc:
                result = {"selected_ids": [], "reason": "harness_error",
                          "provider_attempt": False, "cache_hit": False, "usage": None,
                          "elapsed_ms": 0.0, "error": type(exc).__name__}
            receipt["attempts"] += int(result["provider_attempt"])
            valid = result["reason"] in ("selected", "abstained") and result["provider_attempt"] and not result["cache_hit"]
            row = {"case": case["id"], "domain": case["domain"], "kind": case["kind"],
                   "difficulty": case["difficulty"], "model": MODEL, "result": result,
                   "grade": retention(result["selected_ids"], [c["id"] for c in case["cards"]],
                                      case["host"]) if valid else None}
            receipt["results"].append(row)
            receipt["summary"] = summarize(receipt["results"])
            receipt["status"] = "running" if valid else "stopped"
            write(result_path, receipt)
            if not valid:
                print(json.dumps({"status": "stopped", "reason": result["reason"], "output": str(result_path)}))
                return
    receipt["status"] = "completed"
    write(result_path, receipt)
    print(json.dumps(receipt["summary"], indent=2))


if __name__ == "__main__":
    main()
