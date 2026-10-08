#!/usr/bin/env python3
"""Frozen, provider-free prompt-recall comparison on a disposable Mneme store.

The five source-bound records and six expected-usefulness judgments below are
the evaluation fixture. Do not tune them after inspecting retrieval results.
This measures passive card exposure, not model answer quality.
"""

import argparse
import hashlib
import json
import os
from pathlib import Path
import re
import socket
import subprocess
import sys
import time


REPO = Path(__file__).resolve().parents[1]
INTEGRATION = REPO / "integrations" / "codex"
DEFAULT_OUT = REPO / "target" / "codex-hook-recall-v1" / "eval"

# Source-bound smoke cards. Every reference points into this repo.
CARDS = (
    ("permissions", "integrations/codex/README.md",
     "Trusted Codex hooks, native MCP tool approval, and writable checkpoint filesystem are separate controls; hook trust alone grants neither memory tools nor writes."),
    ("feature_mismatch", "integrations/codex/README.md",
     "Keep the installed mnemed and mneme-mcp embedding-feature pair matched; a hashing CLI beside a BGE host correctly refuses offline reopen with an embedding fingerprint mismatch."),
    ("source_revision", "crates/mneme-engine/tests/capture.rs#checkout_change_replays_original_anchor_but_source_revision_conflicts",
     "Capture source namespace/key is a stable logical identity: identical request replays its ID despite later host HEAD; changed source revision under that key conflicts rather than silently replacing it."),
    ("supersession", "integrations/codex/README.md#source-keyed-capture",
     "A reviewed current-source correction uses a successor plus explicit supersede: the loser is archived with durable history, excluded from ordinary recall, and still available by direct get."),
    ("one_host_lease", "integrations/codex/README.md#service-lease",
     "One shared Mneme host owns the project's exclusive store lease; stop it before using one-shot mnemed CLI against that store."),
)

# Expected IDs are symbolic until capture init/add returns actual ULIDs.
PROMPTS = (
    ("permissions", "Debug why a fresh Codex session has trusted project hooks but cannot recall or checkpoint Mneme; distinguish the permissions without broadening global settings.", ("permissions",)),
    ("feature_mismatch", "Investigate an offline mnemed reopen that reports embedding fingerprint mismatch after parallel feature builds left a hashing CLI beside the BGE MCP host. How should we verify the installed pair?", ("feature_mismatch",)),
    ("source_correction", "Review a captured source claim whose revision changed after commit: should the same namespace/key replay or conflict, and how should a stale claim be retired from ordinary recall?", ("source_revision", "supersession")),
    ("unicode_validation", "Review NodeSummary Unicode byte-limit validation and truncation behavior in the Rust code. Identify boundary cases and appropriate tests; do not change memory deployment.", ()),
    ("hnsw_budget", "Investigate HNSW candidate retrieval budgeting and lifecycle filtering in the Rust vector query path. Find a bounded counterexample and propose a test.", ()),
    ("trivial", "thanks!", ()),
)


def canonical(value):
    return json.dumps(value, sort_keys=True, ensure_ascii=False, separators=(",", ":"))


def write_json(path, value):
    path.write_text(json.dumps(value, indent=2, sort_keys=True, ensure_ascii=False) + "\n", encoding="utf-8")


def run(argv, *, cwd=REPO, payload=None, timeout=30):
    started = time.monotonic()
    result = subprocess.run([str(x) for x in argv], input=payload, text=True,
                            capture_output=True, cwd=cwd, timeout=timeout, check=False)
    elapsed = round((time.monotonic() - started) * 1000)
    if result.returncode:
        raise RuntimeError(f"{argv[0]} failed ({result.returncode}): {result.stderr[-1200:]}")
    return result.stdout, elapsed


def json_run(argv, **kwargs):
    output, elapsed = run(argv, **kwargs)
    return json.loads(output), elapsed


def free_port():
    with socket.socket() as sock:
        sock.bind(("127.0.0.1", 0))
        return sock.getsockname()[1]


def hook(config, event, script=INTEGRATION / "hooks.py"):
    output, elapsed = json_run([sys.executable, script, "--config", config],
                               payload=canonical(event), timeout=5)
    context = output.get("hookSpecificOutput", {}).get("additionalContext", "")
    if not isinstance(context, str):
        raise ValueError("malformed hook context")
    ids = re.findall(r'"id":"([0-7][0-9A-HJKMNP-TV-Z]{25})"', context)
    return {"output": output, "elapsed_ms": elapsed,
            "context_bytes": len(context.encode("utf-8")), "card_ids": ids}


def event(root, kind, session, turn=None, prompt=None, source=None):
    value = {"cwd": str(root), "hook_event_name": kind, "session_id": session}
    if turn is not None:
        value["turn_id"] = turn
    if prompt is not None:
        value["prompt"] = prompt
    if source is not None:
        value["source"] = source
    return value


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--out", type=Path, default=DEFAULT_OUT)
    parser.add_argument("--mnemed", type=Path, default=Path.home() / ".cargo/bin/mnemed")
    parser.add_argument("--mcp", type=Path, default=Path.home() / ".cargo/bin/mneme-mcp")
    parser.add_argument("--installed-v4", action="store_true",
                        help="prepare/apply an automatic v4 private install and invoke copied hooks")
    args = parser.parse_args()
    out = args.out.resolve()
    if out.exists() and any(out.iterdir()):
        parser.error("output directory is nonempty; refusing a fixture-tuning rerun")
    out.mkdir(parents=True, exist_ok=True, mode=0o700)
    os.chmod(out, 0o700)
    fixture = {"cards": CARDS, "prompts": PROMPTS}
    fixture_sha = hashlib.sha256(canonical(fixture).encode()).hexdigest()
    write_json(out / "frozen-fixture.json", {"fixture": fixture, "sha256": fixture_sha})
    root = out / "project"
    db = root / ".mneme" / "codex-memory.db"
    db.parent.mkdir(parents=True)
    write_json(out / "manifest.json", {"fixture_sha256": fixture_sha,
               "mnemed_sha256": hashlib.sha256(args.mnemed.read_bytes()).hexdigest(),
               "mcp_sha256": hashlib.sha256(args.mcp.read_bytes()).hexdigest(),
               "db": str(db), "provider_calls": 0})
    cli = [args.mnemed, "--db", db, "--json"]
    json_run(cli + ["capture", "init"])
    ids = {}
    for name, reference, summary in CARDS:
        request = {"source": {"namespace": "codex-hook-eval", "key": name,
                              "reference": str(REPO / reference.split("#", 1)[0]) + "#" + reference.split("#", 1)[1]},
                   "summary": summary, "active": True}
        result, _ = json_run(cli + ["capture", "add", "--input", "-"], payload=canonical(request))
        ids[name] = result["id"]
    before, _ = json_run(cli + ["status"])
    port = free_port()
    installed = None
    if args.installed_v4:
        if sys.version_info < (3, 11):
            parser.error("v4 installer requires Python 3.11+; run with pinned python3.14")
        sys.path.insert(0, str(INTEGRATION))
        import install
        prefix = out / "private-v4"
        plan = install.prepare(root, prefix, args.mnemed, args.mcp, port,
                               recall_mode="automatic")
        write_json(out / "install-plan.json", plan)
        applied = install.apply(plan)
        hashes = {row["destination"]: {
            "expected": row["sha256"],
            "actual": hashlib.sha256((prefix / row["destination"]).read_bytes()).hexdigest()}
            for row in plan["files"]}
        if len(hashes) != 8 or any(v["expected"] != v["actual"] for v in hashes.values()):
            raise RuntimeError("v4 installed runtime hash mismatch")
        installed = {"plan_sha256": plan["plan_sha256"], "apply": applied,
                     "runtime_hashes": hashes, "all_eight_match": True,
                     "recall_mode": plan["recall_mode"]}
        service_config = prefix / "config/service.json"
        script = prefix / "lib/hooks.py"
        service_script = prefix / "lib/service.py"
    else:
        service_config = out / "service.json"
        write_json(service_config, {"binary": str(args.mcp.resolve()), "project_db": str(db),
                   "port": port, "state_dir": str(out / "service-state"),
                   "working_directory": str(REPO)})
        script = INTEGRATION / "hooks.py"
        service_script = INTEGRATION / "service.py"
    service = [sys.executable, service_script, "--config", service_config]
    configs = {}
    for mode in ("automatic", "reminder"):
        if args.installed_v4 and mode == "automatic":
            config = prefix / "config/hooks.json"
        else:
            config = out / f"hooks-{mode}.json"
            write_json(config, {"schema": "mneme.codex-hooks.config.v2",
                       "project_root": str(root), "state_dir": str(out / f"hook-state-{mode}"),
                       "service_config": str(service_config), "recall_mode": mode})
        configs[mode] = config
    receipt = {"fixture_sha256": fixture_sha, "ids": ids, "before": before,
               "service_port": port, "steps": [], "provider_calls": 0}
    if installed is not None:
        receipt["installed_v4"] = installed
    started = False
    try:
        receipt["service_start"], _ = json_run(service + ["start"])
        started = True
        for mode in ("automatic", "reminder"):
            for index, (name, prompt, expected_names) in enumerate(PROMPTS, 1):
                # Independent tasks must not inherit prior cards' dedup fence.
                session = f"eval-{mode}-{name}"
                receipt["steps"].append({"mode": mode, "name": f"start_{name}", **hook(
                configs[mode], event(root, "SessionStart", session, source="startup"), script)})
                result = hook(configs[mode], event(root, "UserPromptSubmit", session,
                                                   turn=f"turn-{index}", prompt=prompt), script)
                expected = {ids[key] for key in expected_names}
                actual = set(result["card_ids"])
                receipt["steps"].append({"mode": mode, "name": name,
                    "expected_ids": sorted(expected), "useful_ids": sorted(actual & expected),
                    "noise_ids": sorted(actual - expected), **result})
        # Same context must not re-inject a seen card; compact must reset that fence.
        prompt = PROMPTS[0][1]
        session = "eval-automatic-permissions"
        receipt["steps"].append({"mode": "automatic", "name": "repeat",
            **hook(configs["automatic"], event(root, "UserPromptSubmit", session,
                                                 turn="repeat", prompt=prompt), script)})
        receipt["steps"].append({"mode": "automatic", "name": "compact",
            **hook(configs["automatic"], event(root, "SessionStart", session, source="compact"), script)})
        receipt["steps"].append({"mode": "automatic", "name": "after_compact",
            **hook(configs["automatic"], event(root, "UserPromptSubmit", session,
                                                 turn="after-compact", prompt=prompt), script)})
        receipt["service_stop"], _ = json_run(service + ["stop"], timeout=45)
        started = False
        receipt["steps"].append({"mode": "automatic", "name": "host_unavailable",
            **hook(configs["automatic"], event(root, "UserPromptSubmit", session,
                                                 turn="host-down", prompt=PROMPTS[1][1]), script)})
        receipt["after"], _ = json_run(cli + ["status"])
        receipt["service_final"], _ = json_run(service + ["status"])
        receipt["store_counts_unchanged"] = receipt["before"] == receipt["after"]
        receipt["trivial_quiet"] = all(not s["output"] for s in receipt["steps"] if s["name"] == "trivial")
        receipt["max_context_bytes"] = max(s["context_bytes"] for s in receipt["steps"])
        receipt["unavailable_within_deadline"] = (receipt["steps"][-1]["elapsed_ms"] < 2500
                                                   and not receipt["steps"][-1]["card_ids"])
    finally:
        if started:
            try:
                json_run(service + ["stop"], timeout=45)
            except Exception as error:
                receipt["cleanup_error"] = repr(error)
        if args.installed_v4:
            try:
                receipt["uninstall"] = install.uninstall(prefix / "receipt.json")
                receipt["project_codex_configs_removed"] = not any(
                    (root / ".codex" / name).exists() for name in ("config.toml", "hooks.json"))
            except Exception as error:
                receipt["uninstall_error"] = repr(error)
        write_json(out / "receipt.json", receipt)
    print(json.dumps({"receipt": str(out / "receipt.json"),
                      "fixture_sha256": fixture_sha,
                      "store_counts_unchanged": receipt.get("store_counts_unchanged"),
                      "trivial_quiet": receipt.get("trivial_quiet")}, sort_keys=True))


if __name__ == "__main__":
    main()
