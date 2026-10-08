#!/usr/bin/env python3
"""Finite installed-Codex witness for async producer -> ready slot -> PostToolUse.

Modes are explicit. The generic fixture uses no Mneme store; the copied-bundle
mode uses a disposable store/service and pinned fake reader. Actor modes invoke
Codex; preflight modes do not. No mode touches live configuration or retains a
transcript. Receipts cannot overwrite prior evidence.
"""

from __future__ import annotations

import argparse
import hashlib
import json
import os
from pathlib import Path
import shlex
import shutil
import signal
import subprocess
import sys
import tempfile
import time

sys.dont_write_bytecode = True
ROOT = Path(__file__).resolve().parents[1]
sys.path.insert(0, str(ROOT / "integrations/codex"))
sys.path.insert(0, str(ROOT / "tools"))
import install  # noqa: E402
from reflexive_shadow_smoke import cli, free_port, logical_cozo  # noqa: E402

# Resolve the installed executable without assuming a user's home or package
# manager. An unavailable command remains an inert path until an explicit mode.
CODEX = Path(shutil.which("codex") or "codex")
PYTHON = Path(sys.executable)
AUTH = Path(os.environ.get("CODEX_HOME", Path.home() / ".codex")) / "auth.json"
OUT = ROOT / "target/async-reader-v1/host-probe.json"
NONCE = "plum-orbit-7642"
BIN = ROOT / "target/reflexive-shadow-v1/persistent/bin"


def hook_command(root: Path, event: str) -> str:
    return shlex.join([str(PYTHON), str(Path(__file__).resolve()),
                       "--hook", str(root), event])


def digest(path: Path) -> str:
    return hashlib.sha256(path.read_bytes()).hexdigest()


def hook_main(root: Path, event_name: str) -> None:
    # This function executes only as a temporary, hash-bound hook command.
    payload = json.loads(sys.stdin.buffer.read(64_001))
    if not isinstance(payload, dict):
        return
    sid, tid = payload.get("session_id"), payload.get("turn_id")
    row = {"event": event_name, "keys": sorted(payload),
           "session_id": sid if isinstance(sid, str) else None,
           "turn_id": tid if isinstance(tid, str) else None,
           "tool_name": payload.get("tool_name") if event_name == "PostToolUse" else None}
    with (root / "hooks.jsonl").open("a", encoding="utf-8") as log:
        log.write(json.dumps(row, sort_keys=True) + "\n")
    if not isinstance(sid, str) or not isinstance(tid, str):
        return
    slot = root / "ready.json"
    if event_name == "UserPromptSubmit":
        # Fake background reader: enough delay to establish that this is an
        # asynchronous producer, not inline prompt context. No stdout.
        time.sleep(0.15)
        slot.write_text(json.dumps({"session_id": sid, "turn_id": tid,
                                    "nonce": NONCE}), encoding="utf-8")
    elif event_name == "PostToolUse" and slot.exists():
        ready = json.loads(slot.read_text(encoding="utf-8"))
        if ready.get("session_id") == sid and ready.get("turn_id") == tid:
            slot.unlink()
            row = {"session_id": sid, "turn_id": tid, "emitted": True}
            (root / "emission.json").write_text(json.dumps(row), encoding="utf-8")
            print(json.dumps({"hookSpecificOutput": {"hookEventName": "PostToolUse",
                    "additionalContext": "Private test card: the verification nonce is " + NONCE +
                    ". Report it after the tool result."}}))


def fresh_receipt(path: Path) -> None:
    """Refuse an existing receipt before any actor or service starts."""
    path.parent.mkdir(parents=True, exist_ok=True)
    if path.exists():
        raise RuntimeError(f"refusing to overwrite probe receipt: {path}")


def fixture_main(output: Path = OUT) -> None:
    fresh_receipt(output)
    assert CODEX.is_file() and PYTHON.is_file() and AUTH.is_file()
    started = time.monotonic()
    with tempfile.TemporaryDirectory(prefix="mneme-async-host-") as tmp:
        root = Path(tmp).resolve()
        project = root / "project"
        project.mkdir()
        home = root / "codex-home"
        home.mkdir(mode=0o700)
        (home / "auth.json").symlink_to(AUTH)
        hooks = {"hooks": {
            "UserPromptSubmit": [{"hooks": [{"type": "command", "async": True,
                "timeout": 5, "command": hook_command(root, "UserPromptSubmit")}]}],
            "PostToolUse": [{"matcher": "Bash", "hooks": [{"type": "command",
                "timeout": 5, "additionalContextLimit": 200,
                "command": hook_command(root, "PostToolUse")}]}],
        }}
        hook_file = home / "hooks.json"
        hook_file.write_text(json.dumps(hooks, indent=2) + "\n", encoding="utf-8")
        env = os.environ.copy()
        env["CODEX_HOME"] = str(home)
        env["GIT_CEILING_DIRECTORIES"] = str(root)
        for key in ("MNEME_DB", "OPENAI_API_KEY", "MNEME_SERVICE_CONFIG"):
            env.pop(key, None)
        answer = root / "answer.txt"
        prompt = ("This is a finite hook delivery test in a disposable empty directory. "
                  "Run one Bash command exactly: `sleep 2; printf 'boundary\\n'`. "
                  "Then answer with the verification nonce if a hook supplies one after the "
                  "tool result; otherwise say NO_NONCE. Do not inspect files, env, or hooks. "
                  "The nonce is not in this prompt or the Bash output.")
        argv = [str(CODEX), "exec", "--dangerously-bypass-hook-trust", "--ephemeral",
                "--skip-git-repo-check", "--ignore-rules", "--sandbox", "read-only",
                "--disable", "memories", "--disable", "multi_agent",
                "-c", "project_doc_max_bytes=0", "-c", 'model_reasoning_effort="low"',
                "-m", "gpt-5.6-sol", "--json", "-o", str(answer), "-C", str(project), "-"]
        process = subprocess.Popen(argv, cwd=project, env=env, stdin=subprocess.PIPE,
                                   stdout=subprocess.PIPE, stderr=subprocess.PIPE,
                                   start_new_session=True)
        try:
            stdout, stderr = process.communicate(prompt.encode(), timeout=45)
        except subprocess.TimeoutExpired:
            os.killpg(process.pid, signal.SIGKILL)
            process.communicate()
            raise RuntimeError("45-second Codex actor deadline exceeded")
        events = [json.loads(line) for line in stdout.splitlines() if line.strip()]
        hook_rows = [json.loads(line) for line in (root / "hooks.jsonl").read_text().splitlines()] if (root / "hooks.jsonl").exists() else []
        emission = json.loads((root / "emission.json").read_text()) if (root / "emission.json").exists() else None
        actual_answer = answer.read_text().strip() if answer.exists() else None
        identities = [(r["event"], r["session_id"], r["turn_id"]) for r in hook_rows]
        passed = (process.returncode == 0 and emission is not None and
                  NONCE in (actual_answer or "") and len(identities) >= 2 and
                  any(x[0] == "UserPromptSubmit" and x[1:] ==
                      (emission["session_id"], emission["turn_id"]) for x in identities) and
                  any(x[0] == "PostToolUse" and x[1:] ==
                      (emission["session_id"], emission["turn_id"]) for x in identities))
        report = {"schema": "mneme.async-reader-host-probe.v1", "status": "passed" if passed else "failed",
                  "fixture_not_production_bundle": True, "codex_version": subprocess.run(
                      [str(CODEX), "--version"], capture_output=True, text=True, check=True).stdout.strip(),
                  "manifest": {"codex_sha256": digest(CODEX), "hook_program_sha256": digest(Path(__file__).resolve()),
                               "hooks_config_sha256": digest(hook_file)},
                  "isolated_home": True, "auth_symlink_only": True,
                  "model": "gpt-5.6-sol", "effort": "low", "codex_invocations": 1,
                  "elapsed_ms": round((time.monotonic() - started) * 1000),
                  "actor_exit": process.returncode,
                  "actor_event_types": sorted({r.get("type") for r in events}),
                  "actor_item_types": sorted({r.get("item", {}).get("type") for r in events
                                              if isinstance(r.get("item"), dict)}),
                  "answer": actual_answer, "nonce_seen_in_answer": NONCE in (actual_answer or ""),
                  "hook_rows": hook_rows, "emission": emission,
                  "stderr_tail": stderr.decode(errors="replace")[-400:]}
        with output.open("x", encoding="utf-8") as stream:
            stream.write(json.dumps(report, indent=2) + "\n")
        print(json.dumps({"status": report["status"], "output": str(output),
                          "identity_pairs": identities, "nonce_seen_in_answer": report["nonce_seen_in_answer"]}))
        if not passed:
            raise SystemExit(1)


def fake_reader(path: Path, trace: Path) -> None:
    """Pinned fake app-server: selects first native ID, no provider or key read."""
    source = '''#!%s
import hashlib, json, os, pathlib, sys
trace = pathlib.Path(%r)
def emit(row):
    print(json.dumps(row, separators=(",", ":")), flush=True)
for raw in sys.stdin:
    try:
        message = json.loads(raw)
    except ValueError:
        continue
    method = message.get("method")
    ident = message.get("id")
    if ident is None:
        continue
    params = message.get("params", {})
    if method == "initialize":
        emit({"id": ident, "result": {}})
    elif method == "thread/start":
        emit({"id": ident, "result": {"thread": {"id": "fake-thread", "ephemeral": True}, "model": "gpt-5.6-sol", "approvalPolicy": "never", "instructionSources": []}})
    elif method == "turn/start":
        prompt = params["input"][0]["text"]
        payload = json.loads(prompt.split("PAYLOAD:\\n", 1)[1])
        card = payload["cards"][0]
        with trace.open("a") as out:
            out.write(json.dumps({"method": method, "pid": os.getpid(), "selected_id": card["id"], "card_count": len(payload["cards"]), "prompt_sha256": hashlib.sha256(prompt.encode()).hexdigest()}) + "\\n")
        emit({"id": ident, "result": {"turn": {"id": "fake-reader-turn"}}})
        usage = {"inputTokens": 100, "cachedInputTokens": 0, "cacheWriteInputTokens": 0, "outputTokens": 10, "reasoningOutputTokens": 0, "totalTokens": 110}
        emit({"method": "thread/tokenUsage/updated", "params": {"turnId": "fake-reader-turn", "tokenUsage": {"last": usage, "total": usage}}})
        emit({"method": "item/completed", "params": {"item": {"type": "agentMessage", "text": json.dumps({"selected_ids": [card["id"]]})}}})
        emit({"method": "turn/completed", "params": {"turn": {"id": "fake-reader-turn", "status": "completed"}}})
    else:
        emit({"id": ident, "error": {"code": -32601, "message": "unsupported"}})
''' % (PYTHON, str(trace))
    path.write_text(source, encoding="utf-8")
    path.chmod(0o700)


def bundle_main(*, preflight: bool = False, current: bool = False,
                output: Path | None = None) -> None:
    """Run the actual installed async bundle with native store and fake reader."""
    output = output or OUT.with_name(
        "host-probe-preflight-current.json" if current else
        "host-probe-preflight.json" if preflight else "host-probe-installed.json")
    fresh_receipt(output)
    for path in (CODEX, PYTHON, AUTH, BIN / "mnemed", BIN / "mneme-mcp"):
        if not path.is_file():
            raise RuntimeError(f"missing prerequisite: {path}")
    started = time.monotonic()
    with tempfile.TemporaryDirectory(prefix="mneme-async-bundle-") as tmp:
        root = Path(tmp).resolve()
        project = root / "project"
        project.mkdir()
        (project / ".mneme").mkdir()
        home = root / "codex-home"
        home.mkdir(mode=0o700)
        (home / "auth.json").symlink_to(AUTH)
        env = os.environ.copy()
        env.update({"CODEX_HOME": str(home), "GIT_CEILING_DIRECTORIES": str(root),
                    "HF_HUB_OFFLINE": "1", "TRANSFORMERS_OFFLINE": "1"})
        for key in ("MNEME_DB", "OPENAI_API_KEY", "MNEME_SERVICE_CONFIG"):
            env.pop(key, None)
        cli(BIN / "mnemed", project, env, "capture", "init")
        gate = cli(BIN / "mnemed", project, env, "capture", "add", "--input", "-",
                   payload={"source": {"namespace": "host-probe", "key": "gate",
                                       "reference": "fixture://host-probe/gate"},
                            "summary": "Archive keeper phrase: " + NONCE + ". Keep the gate closed.",
                            "body": "This disposable test fact says the keeper phrase is " + NONCE +
                                    " and the gate must remain closed.", "active": True})["id"]
        before = logical_cozo(project / ".mneme/codex-memory.db")
        reader = root / "fake-app-server"
        trace = root / "reader-trace.jsonl"
        fake_reader(reader, trace)
        prefix = root / "private-prefix"
        plan = install.prepare(project, prefix, BIN / "mnemed", BIN / "mneme-mcp",
                               free_port(), recall_mode="async", reader_model="gpt-5.6-sol",
                               reader_codex=reader)
        install.apply(plan)
        hook_file = project / ".codex/hooks.json"
        home_hook = home / "hooks.json"
        home_hook.write_bytes(hook_file.read_bytes())
        (project / ".codex/config.toml").unlink()
        hook_file.unlink()
        files_match = all(digest(prefix / row["destination"]) == row["sha256"]
                          for row in plan["files"])
        if not files_match:
            raise RuntimeError("installed bundle hash mismatch")
        service_cfg = prefix / "config/service.json"
        service = prefix / "lib/service.py"
        running = False
        try:
            start_service = subprocess.run([str(PYTHON), str(service), "--config", str(service_cfg), "start"],
                                           cwd=project, env=env, capture_output=True, text=True, timeout=30)
            if start_service.returncode != 0:
                raise RuntimeError("disposable service failed to start: " + start_service.stderr[-300:])
            running = True
            if preflight:
                cue = "Current task: Investigate archive gate keeper phrase and decide whether to open it."
                # Execute from the copied bundle path, not the mutable source
                # import already present in this harness's Python process.
                preflight_code = """import json, sys
from pathlib import Path
import hooks, hook_recall, reader_worker
from reader_runtime import ReaderRuntime
config = hooks._config(Path(sys.argv[1]))
pool = hook_recall.collect_reader(Path(sys.argv[2]), sys.argv[4], Path(sys.argv[3]), timeout=1.5)
cards = pool.get('cards', []) if isinstance(pool, dict) else []
if not cards:
    raise SystemExit('native pool empty')
with ReaderRuntime(config, Path(sys.argv[5])) as runtime:
    selected = runtime.select([{'role': 'user', 'text': sys.argv[4]}], cards)
session, turn = 'host-probe-session', 'host-probe-turn'
base = {'cwd': sys.argv[3], 'session_id': session, 'turn_id': turn}
noticed = reader_worker.notice(config, {**base, 'prompt': sys.argv[4]}, True)
def ready(data):
    pending = data['pending']
    data['pending'] = None
    data['ready'] = {**pending, 'cards': [cards[0]], 'fence': data['fence']}
    return None, True
injected, _ = reader_worker._state(config, session, ready, create=False)
wrong = hooks.handle_event({**base, 'turn_id': 'host-probe-other', 'hook_event_name': 'PostToolUse'}, config)
right = hooks.handle_event({**base, 'hook_event_name': 'PostToolUse'}, config)
wrong_context = wrong.get('hookSpecificOutput', {}).get('additionalContext', '')
right_context = right.get('hookSpecificOutput', {}).get('additionalContext', '')
print(json.dumps({'pool_outcome': pool.get('outcome'), 'pool_card_ids': [c.get('id') for c in cards],
                  'reader': selected, 'notice_outcome': noticed.get('outcome'),
                  'slot_injected': injected, 'wrong_turn_rejected': not bool(wrong_context),
                  'right_turn_emitted': 'plum-orbit-7642' in right_context}))
"""
                pre_env = {**env, "PYTHONPATH": str(prefix / "lib")}
                pre = subprocess.run([str(PYTHON), "-c", preflight_code,
                                      str(prefix / "config/hooks.json"), str(service_cfg),
                                      str(project), cue, str(root / "reader-scratch")],
                                     cwd=project, env=pre_env, capture_output=True,
                                     text=True, timeout=10)
                if pre.returncode != 0:
                    raise RuntimeError("copied-bundle preflight failed: " + pre.stderr[-300:])
                outcome = json.loads(pre.stdout)
                selected = outcome["reader"]
                trace_rows = [json.loads(line) for line in trace.read_text().splitlines()] if trace.exists() else []
                fake_pid = trace_rows[0].get("pid") if trace_rows else None
                fake_process_gone = False
                if isinstance(fake_pid, int) and fake_pid > 0:
                    try:
                        os.kill(fake_pid, 0)
                    except ProcessLookupError:
                        fake_process_gone = True
                stop_service = subprocess.run([str(PYTHON), str(service), "--config", str(service_cfg), "stop"],
                                              cwd=project, env=env, capture_output=True, text=True, timeout=30)
                if stop_service.returncode != 0:
                    raise RuntimeError("disposable service failed to stop: " + stop_service.stderr[-300:])
                running = False
                # Successful independent reopen after stop is evidence that the
                # native store lease was released, not merely that a PID exited.
                lease_reopen = cli(BIN / "mnemed", project, env, "status")
                after = logical_cozo(project / ".mneme/codex-memory.db")
                build_manifest = ROOT / "target/reflexive-shadow-v1/build-inventory.json"
                build = json.loads(build_manifest.read_text(encoding="utf-8"))
                native = {name: {"sha256": digest(BIN / name),
                                 "matches_build_manifest": digest(BIN / name) ==
                                     build["artifacts"]["persistent"]["binaries"][name]["sha256"]}
                          for name in ("mnemed", "mneme-mcp")}
                check = {"schema": "mneme.async-reader-bundle-preflight.v1",
                         "provider_calls": 0, "copied_bundle_files_match": files_match,
                         "plan_sha256": plan["plan_sha256"],
                         "installed_file_hashes": {row["destination"]: digest(prefix / row["destination"])
                                                   for row in plan["files"]},
                         "installed_hooks_config_sha256": digest(home_hook),
                         "build_manifest_sha256": digest(build_manifest),
                         "native_build": {"source_commit": build["source_commit"],
                                          "features": build["artifacts"]["persistent"]["features"],
                                          "mnemed_version": subprocess.run([str(BIN / "mnemed"), "--version"],
                                              capture_output=True, text=True, check=True).stdout.strip(),
                                          "mneme_mcp_version": "--version unsupported",
                                          "binaries": native},
                         "installed_program_hashes": {name: digest(prefix / "lib" / name)
                             for name in ("hooks.py", "reader_worker.py", "reader_runtime.py",
                                          "reader_contract.py", "hook_recall.py")},
                         "pool_outcome": outcome["pool_outcome"], "pool_card_ids": outcome["pool_card_ids"],
                         "reader": selected, "fixture_id": gate, "reader_binary_sha256": digest(reader),
                         "installed_hook_refusal": {k: outcome[k] for k in
                             ("notice_outcome", "slot_injected", "wrong_turn_rejected", "right_turn_emitted")},
                         "cleanup": {"service_stop_exit": stop_service.returncode,
                                     "store_lease_reopen": isinstance(lease_reopen, dict),
                                     "fake_reader_process_gone": fake_process_gone,
                                     "logical_kv_equal": before == after}}
                with output.open("x", encoding="utf-8") as stream:
                    stream.write(json.dumps(check, indent=2) + "\n")
                print(json.dumps({"output": str(output), "selected": selected.get("selected_ids"),
                                  "reason": selected.get("reason")}))
                if (gate not in selected.get("selected_ids", []) or not fake_process_gone
                        or before != after or not all(native[name]["matches_build_manifest"] for name in native)
                        or not outcome["wrong_turn_rejected"] or not outcome["right_turn_emitted"]):
                    raise RuntimeError("copied-bundle preflight did not satisfy its checks")
                return
            answer = root / "answer.txt"
            prompt = ("Investigate the archive gate keeper phrase in this disposable project's memory. "
                      "Run one Bash command first, exactly `sleep 8; printf 'boundary\\n'`. "
                      "In your first response after that command, give the exact keeper phrase and say "
                      "whether the gate should be opened. Do not let a later memory checkpoint "
                      "replace that answer; if asked to checkpoint afterward, do so separately. "
                      "Do not inspect files, environment, hooks, or run other tools. "
                      "If no memory arrives, say UNKNOWN rather than guess.")
            argv = [str(CODEX), "exec", "--dangerously-bypass-hook-trust", "--ephemeral",
                    "--skip-git-repo-check", "--ignore-rules", "--sandbox", "workspace-write",
                    "--disable", "memories", "--disable", "multi_agent",
                    "-c", "project_doc_max_bytes=0", "-c", 'model_reasoning_effort="low"',
                    "-m", "gpt-5.6-sol", "--json", "-o", str(answer), "-C", str(project), "-"]
            process = subprocess.Popen(argv, cwd=project, env=env, stdin=subprocess.PIPE,
                                       stdout=subprocess.PIPE, stderr=subprocess.PIPE,
                                       start_new_session=True)
            try:
                stdout, stderr = process.communicate(prompt.encode(), timeout=45)
            except subprocess.TimeoutExpired:
                os.killpg(process.pid, signal.SIGKILL)
                process.communicate()
                raise RuntimeError("45-second installed actor deadline exceeded")
            events = [json.loads(line) for line in stdout.splitlines() if line.strip()]
            boundary = next((i for i, row in enumerate(events)
                             if row.get("type") == "item.completed" and
                             isinstance(row.get("item"), dict) and
                             row["item"].get("type") == "command_execution"), None)
            nonce_after_tool = (boundary is not None and any(
                NONCE in row.get("item", {}).get("text", "")
                for row in events[boundary + 1:]
                if row.get("type") == "item.completed" and
                isinstance(row.get("item"), dict) and
                row["item"].get("type") == "agent_message"))
            actual_answer = answer.read_text().strip() if answer.exists() else None
            reader_rows = [json.loads(line) for line in trace.read_text().splitlines()] if trace.exists() else []
            state_dir = project / ".mneme/codex-hook-state/reader"
            state_paths = [p for p in state_dir.glob("*.json") if p.is_file()] if state_dir.exists() else []
            state = json.loads(state_paths[0].read_text()) if len(state_paths) == 1 else None
            after = logical_cozo(project / ".mneme/codex-memory.db")
            passed = (process.returncode == 0 and files_match and gate in
                      [row.get("selected_id") for row in reader_rows] and
                      nonce_after_tool and state is not None and
                      any(gate == pair[0] for pair in state.get("emitted", []) if isinstance(pair, list)) and
                      before == after)
            report = {"schema": "mneme.async-reader-installed-host-probe.v1",
                      "status": "passed" if passed else "failed", "fixture_reader_not_real_provider": True,
                      "codex_version": subprocess.run([str(CODEX), "--version"], text=True,
                          capture_output=True, check=True).stdout.strip(),
                      "manifest": {"codex_sha256": digest(CODEX), "fake_reader_sha256": digest(reader),
                                   "plan_sha256": plan["plan_sha256"], "hooks_config_sha256": digest(home_hook),
                                   "harness_sha256": digest(Path(__file__).resolve()),
                                   "installed_program_hashes": {name: digest(prefix / "lib" / name)
                                       for name in ("hooks.py", "reader_worker.py", "reader_runtime.py",
                                                    "reader_contract.py", "hook_recall.py", "install.py")
                                       if (prefix / "lib" / name).exists()},
                                   "bundle_files_match": files_match},
                      "isolated_home": True, "auth_symlink_only": True, "model": "gpt-5.6-sol",
                      "effort": "low", "codex_invocations": 1,
                      "elapsed_ms": round((time.monotonic() - started) * 1000),
                      "actor_exit": process.returncode,
                      "actor_item_types": sorted({r.get("item", {}).get("type") for r in events
                                                  if isinstance(r.get("item"), dict)}),
                      "tool_boundary_seen": boundary is not None,
                      "nonce_in_agent_message_after_tool": nonce_after_tool,
                      "answer": actual_answer, "nonce_seen_in_answer": NONCE in (actual_answer or ""),
                      "reader_rows": reader_rows, "fixture_card_id": gate,
                      "reader_state": {k: state.get(k) for k in ("attempts", "input_tokens", "output_tokens",
                                                                  "unknown_usage", "emitted")}
                          | {"active": {k: state["active"].get(k) for k in ("turn", "request", "epoch", "closed")}
                             if isinstance(state.get("active"), dict) else None,
                             "ready_card_ids": [c.get("id") for c in state.get("ready", {}).get("cards", [])]
                             if isinstance(state.get("ready"), dict) else []}
                          if state is not None else None,
                      "persistent_kv": {"before": before, "after": after, "equal": before == after},
                      "actor_error_count": sum(r.get("item", {}).get("type") == "error" for r in events
                                               if isinstance(r.get("item"), dict))}
            with output.open("x", encoding="utf-8") as stream:
                stream.write(json.dumps(report, indent=2) + "\n")
            print(json.dumps({"status": report["status"], "output": str(output),
                              "nonce_seen_in_answer": report["nonce_seen_in_answer"],
                              "reader_turns": len(reader_rows)}))
            if not passed:
                raise SystemExit(1)
        finally:
            # Ephemeral Codex sessions need not fire SessionEnd immediately.
            # Ask every disposable worker to shut down, then enforce cleanup.
            state_dir = project / ".mneme/codex-hook-state/reader"
            if state_dir.exists():
                import reader_worker
                import hooks
                config = hooks._config(prefix / "config/hooks.json")
                thread_id = next((r.get("thread_id") for r in locals().get("events", [])
                                  if r.get("type") == "thread.started"), None)
                if isinstance(thread_id, str):
                    reader_worker.end_session(config, thread_id)
                pids = []
                for path in state_dir.glob("*.json"):
                    try:
                        pid = json.loads(path.read_text()).get("worker_pid")
                        if isinstance(pid, int) and pid > 0:
                            pids.append(pid)
                    except (ValueError, OSError):
                        pass
                time.sleep(0.2)
                for pid in pids:
                    try:
                        os.killpg(pid, signal.SIGKILL)
                    except ProcessLookupError:
                        pass
            if running:
                subprocess.run([str(PYTHON), str(service), "--config", str(service_cfg), "stop"],
                               cwd=project, env=env, capture_output=True, timeout=30)


def main(argv: list[str] | None = None) -> None:
    parser = argparse.ArgumentParser(description=__doc__)
    mode = parser.add_mutually_exclusive_group(required=True)
    mode.add_argument("--fixture", action="store_true", help="one real Codex actor with fixture hooks")
    mode.add_argument("--bundle", action="store_true", help="one real Codex actor with copied bundle")
    mode.add_argument("--bundle-preflight", action="store_true", help="provider-free copied-bundle preflight")
    mode.add_argument("--bundle-preflight-current", action="store_true",
                      help="provider-free current copied-bundle preflight")
    mode.add_argument("--hook", nargs=2, metavar=("ROOT", "EVENT"),
                      help="internal temporary hook entrypoint")
    parser.add_argument("--output", type=Path, help="new receipt path; existing files are refused")
    args = parser.parse_args(argv)
    if args.hook:
        if args.output is not None:
            parser.error("--output is not valid with --hook")
        hook_main(Path(args.hook[0]), args.hook[1])
    elif args.fixture:
        fixture_main(args.output or OUT)
    else:
        bundle_main(preflight=args.bundle_preflight or args.bundle_preflight_current,
                    current=args.bundle_preflight_current, output=args.output)


if __name__ == "__main__":
    main()
