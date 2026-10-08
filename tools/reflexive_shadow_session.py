#!/usr/bin/env python3
"""Finite real Codex hook-lifecycle witness for the disposable shadow pilot.

This makes two provider calls at most: one outer fresh Codex turn and one
asynchronous assessor. It never reads/copies auth.json, changes a live Codex
home, or retains a transcript. `--dangerously-bypass-hook-trust` applies only
to the generated and SHA-256-recorded hook in an isolated temporary home.
"""

from __future__ import annotations

import argparse
import hashlib
import json
import os
from pathlib import Path
import signal
import subprocess
import sys
import tempfile
import time

sys.dont_write_bytecode = True
ROOT = Path(__file__).resolve().parents[1]
sys.path.insert(0, str(ROOT / "integrations/codex"))
import install  # noqa: E402
from reflexive_shadow_smoke import cli, free_port, logical_cozo, sha  # noqa: E402

PYTHON = Path(sys.executable).resolve()
CODEX = Path.home() / ".local/bin/codex"
AUTH = Path(os.environ.get("CODEX_HOME", Path.home() / ".codex")) / "auth.json"
BIN = ROOT / "target/reflexive-shadow-v1/persistent/bin"
PROMPT = ("Investigate the archive gate keeper phrase in this disposable project's "
          "memory. Give the exact phrase, and say whether the gate should be opened. "
          "Do not guess if the memory is unavailable; do not run tools or inspect files.")
PHRASE = "violet badger lantern 4827"
MAX_SECONDS = 120


def need(ok: bool, message: str) -> None:
    if not ok:
        raise RuntimeError(message)


def checked(argv: list[str | Path], *, cwd: Path, env: dict, timeout: float = 45) -> dict:
    result = subprocess.run([str(x) for x in argv], cwd=cwd, env=env,
                            text=True, capture_output=True, timeout=timeout)
    need(result.returncode == 0, f"{Path(str(argv[0])).name} exit={result.returncode}: {result.stderr[-500:]}")
    return json.loads(result.stdout)


def codex_turn(project: Path, home: Path, env: dict, output: Path, deadline: float) -> dict:
    argv = [str(CODEX), "exec", "--dangerously-bypass-hook-trust", "--ephemeral",
            "--skip-git-repo-check", "--ignore-rules", "--sandbox", "read-only",
            "--disable", "memories", "--disable", "multi_agent",
            "--disable", "shell_tool", "-c", "project_doc_max_bytes=0",
            "-c", 'model_reasoning_effort="low"', "-m", "gpt-5.6-sol",
            "--json", "-o", str(output), "-C", str(project), "-"]
    started = time.monotonic()
    process = subprocess.Popen(argv, cwd=project, env=env, stdin=subprocess.PIPE,
                               stdout=subprocess.PIPE, stderr=subprocess.PIPE,
                               start_new_session=True)
    try:
        stdout, stderr = process.communicate(PROMPT.encode(), timeout=max(1, deadline - time.monotonic()))
    except subprocess.TimeoutExpired as exc:
        os.killpg(process.pid, signal.SIGKILL)
        process.communicate()
        raise RuntimeError("outer Codex turn exceeded finite deadline") from exc
    need(process.returncode == 0, f"outer Codex exit={process.returncode}: {stderr[-900:].decode(errors='replace')}")
    need(len(stdout) <= 1_000_000, "outer Codex event stream exceeded 1 MiB")
    events = []
    for line in stdout.splitlines():
        if line.strip():
            row = json.loads(line)
            need(isinstance(row, dict) and isinstance(row.get("type"), str), "malformed Codex JSON event")
            events.append(row)
    item_types = [row.get("item", {}).get("type") for row in events
                  if row["type"].startswith("item.") and isinstance(row.get("item"), dict)]
    error_items = [row["item"] for row in events if row["type"].startswith("item.")
                   and isinstance(row.get("item"), dict) and row["item"].get("type") == "error"]
    need(all(kind in ("agent_message", "error") for kind in item_types),
         f"outer Codex used an unexpected tool/item: {item_types!r}")
    answer = output.read_text(encoding="utf-8").strip()
    usage = [row.get("usage") for row in events if row.get("type") == "turn.completed"]
    return {"answer": answer, "elapsed_ms": round((time.monotonic() - started) * 1000),
            "event_types": sorted({row["type"] for row in events}),
            "item_types": sorted(set(item_types)),
            "error_items": error_items[:3],
            "usage": usage[-1] if usage else None,
            "stderr_tail": stderr[-500:].decode(errors="replace")}


def wait_sidecar(state: Path, deadline: float) -> dict:
    directory = state / "shadow"
    while time.monotonic() < deadline:
        paths = list(directory.glob("*.json")) if directory.is_dir() else []
        if paths:
            need(len(paths) == 1, "more than one assessor sidecar")
            return json.loads(paths[0].read_text(encoding="utf-8"))
        time.sleep(0.15)
    raise RuntimeError("Stop did not produce a shadow sidecar before deadline")


def main() -> None:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--output", required=True, type=Path,
                        help="JSON evidence path; contains no credential or transcript")
    args = parser.parse_args()
    for path in (CODEX, BIN / "mnemed", BIN / "mneme-mcp"):
        need(path.is_file() and os.access(path, os.X_OK), f"missing executable: {path}")
    need(AUTH.is_file(), "existing Codex auth file is unavailable")
    evidence_path = args.output.absolute()
    evidence_path.parent.mkdir(parents=True, exist_ok=True)
    started = time.monotonic()
    deadline = started + MAX_SECONDS
    evidence = {"schema": "mneme.reflexive-shadow-session.v1", "status": "running",
                "source_commit": subprocess.run(["git", "rev-parse", "HEAD"], cwd=ROOT,
                    text=True, capture_output=True, check=True).stdout.strip(),
                "sources": {"harness_sha256": sha(Path(__file__).resolve()),
                            "mnemed_sha256": sha(BIN / "mnemed"),
                            "mcp_sha256": sha(BIN / "mneme-mcp"),
                            "codex_sha256": sha(CODEX),
                            "codex_version": subprocess.run([str(CODEX), "--version"],
                                text=True, capture_output=True, timeout=5, check=True).stdout.strip()},
                "hook_docs": "https://learn.chatgpt.com/docs/hooks"}
    with tempfile.TemporaryDirectory(prefix="mneme-real-shadow-") as name:
        # macOS /var is a symlink to /private/var; hook config canonicalizes
        # the root, and the service catalog compares exact database paths.
        root = Path(name).resolve()
        project = root / "project"
        project.mkdir()
        (project / ".mneme").mkdir()
        (project / ".mneme/profile.json").write_text(
            '{"schema":"mneme.profile.v1","mode":"isolated"}\n', encoding="utf-8")
        home = root / "codex-home"
        home.mkdir(mode=0o700)
        (home / "auth.json").symlink_to(AUTH)
        env = os.environ.copy()
        env.pop("MNEME_DB", None)
        env.update({"CODEX_HOME": str(home), "MNEME_RERANK": "0",
                    "HF_HUB_OFFLINE": "1", "TRANSFORMERS_OFFLINE": "1",
                    "GIT_CEILING_DIRECTORIES": str(root)})
        db = project / ".mneme/codex-memory.db"
        prefix = root / "private-prefix"
        cli(BIN / "mnemed", project, env, "capture", "init")
        gate = cli(BIN / "mnemed", project, env, "capture", "add", "--input", "-",
                   payload={"source": {"namespace": "shadow-session", "key": "gate",
                                       "reference": "fixture://shadow-session/gate"},
                            "summary": "Archive gate keeper phrase: " + PHRASE + ". Do not open the gate.",
                            "body": "Disposable test fact only. The keeper phrase is " + PHRASE +
                                    "; gate instruction: keep closed.", "active": True})["id"]
        cli(BIN / "mnemed", project, env, "capture", "add", "--input", "-",
            payload={"source": {"namespace": "shadow-session", "key": "map",
                                "reference": "fixture://shadow-session/map"},
                     "summary": "Archive gate map points to the keeper phrase record.",
                     "body": "Follow the linked gate record for the exact phrase.",
                     "links": [{"to": gate, "kind": "associative", "weight": 0.5}],
                     "active": True})
        wrapper = root / "counted-codex"
        count = root / "assessor-count"
        wrapper.write_text("#!/bin/sh\nprintf '1\\n' >> " + repr(str(count)) + "\nexec " +
                           repr(str(CODEX)) + " \"$@\"\n", encoding="utf-8")
        wrapper.chmod(0o700)
        plan = install.prepare(project, prefix, BIN / "mnemed", BIN / "mneme-mcp",
                               free_port(), recall_mode="shadow", shadow_model="gpt-5.6-sol",
                               shadow_codex=wrapper)
        install.apply(plan)
        hook_file = project / ".codex/hooks.json"
        home_hook = home / "hooks.json"
        home_hook.write_bytes(hook_file.read_bytes())
        # This disposable project must not load its project MCP registration or
        # a second copy of the hook; only the vetted temporary home hook runs.
        (project / ".codex/config.toml").unlink()
        hook_file.unlink()
        evidence["install"] = {"plan_sha256": plan["plan_sha256"],
                               "hook_definition_sha256": sha(home_hook),
                               "hook_program_sha256": sha(prefix / "lib/hooks.py"),
                               "shadow_program_sha256": sha(prefix / "lib/shadow.py"),
                               "assessor_wrapper_sha256": sha(wrapper),
                               "files_match": all(sha(prefix / row["destination"]) == row["sha256"]
                                                  for row in plan["files"]),
                               "isolated_profile": True,
                               "temporary_codex_home": True,
                               "auth_symlink_only": True}
        need(evidence["install"]["files_match"], "installed bundle hash mismatch")
        service_cfg = prefix / "config/service.json"
        service = prefix / "lib/service.py"
        running = False
        try:
            checked([PYTHON, service, "--config", service_cfg, "start"], cwd=project, env=env)
            running = True
            before = logical_cozo(db)
            turn = codex_turn(project, home, env, root / "last-answer.txt", deadline)
            evidence["codex_turn"] = turn
            state = project / ".mneme/codex-hook-state"
            state_json = next(state.glob("*.json"), None)
            state_turns = (json.loads(state_json.read_text()).get("turns")
                           if state_json is not None else None)
            need(PHRASE in turn["answer"],
                 "final answer did not contain the memory-only phrase: " + repr(turn["answer"][:500]) +
                 "; events=" + repr(turn["event_types"]) +
                 "; hook_state_files=" + repr([p.name for p in state.glob('*')]) +
                 "; state_turns=" + repr(state_turns)[:700] +
                 "; stderr=" + repr(turn["stderr_tail"]))
            sidecar = wait_sidecar(state, deadline)
            need(sidecar.get("schema") == "mneme.codex-shadow.v1", "invalid sidecar schema")
            need(sidecar.get("status") == "observed", f"assessor did not complete: {sidecar.get('status')}")
            calls = count.read_text(encoding="utf-8").splitlines() if count.exists() else []
            need(len(calls) == 1, f"expected exactly one assessor Codex exec, saw {len(calls)}")
            after = logical_cozo(db)
            need(after == before, "shadow changed persistent Cozo KV")
            evidence["assessment"] = {"status": sidecar["status"],
                                      "judgments": sidecar.get("judgments", []),
                                      "delivered_ids": [row["id"] for row in sidecar.get("delivered", [])],
                                      "assessor_provider_exec_count": len(calls),
                                      "total_provider_exec_count": 1 + len(calls)}
            evidence["persistent_kv"] = {"before": before, "after": after, "equal": True}
            evidence["elapsed_ms"] = round((time.monotonic() - started) * 1000)
            evidence["status"] = "passed"
        finally:
            if running:
                checked([PYTHON, service, "--config", service_cfg, "stop"], cwd=project,
                        env=env, timeout=30)
    evidence_path.write_text(json.dumps(evidence, indent=2) + "\n", encoding="utf-8")
    print(json.dumps({"status": evidence["status"], "output": str(evidence_path),
                      "elapsed_ms": evidence["elapsed_ms"]}))


if __name__ == "__main__":
    main()
