#!/usr/bin/env python3
"""Disposable installed-bundle/native adapter for async-task-v1.

Entering a fixture seeds/clones a store and checks service readiness; it never
retrieves a case or calls a provider. Host-only data lives above the scene-only
actor cwd. Call ``collect``/``native_baseline`` explicitly for native reads.
"""
from __future__ import annotations

from contextlib import closing, contextmanager
from dataclasses import dataclass, field
import fcntl
import hashlib
import json
import os
from pathlib import Path, PurePosixPath
import re
import shutil
import signal
import sqlite3
import subprocess
import sys
import time

sys.dont_write_bytecode = True
ROOT = Path(__file__).resolve().parents[1]
sys.path.insert(0, str(ROOT / "integrations/codex"))
import install  # noqa: E402
from reflexive_shadow_smoke import cli, free_port, logical_cozo, sha  # noqa: E402

CACHE = Path.home() / ".local/share/mneme/.fastembed_cache"
BUILD_MANIFEST = ROOT / "target/reflexive-shadow-v1/build-inventory.json"
MODEL = "gpt-5.6-sol"
BGE_ID = "fastembed:Xenova/bge-base-en-v1.5:onnx-fp32:mneme-adapter-v1"
MAX_HELPER_OUTPUT = 32_768


def need(ok, message):
    if not ok:
        raise RuntimeError(message)


def _json(path, value):
    path.write_text(json.dumps(value, sort_keys=True, ensure_ascii=False, indent=2) + "\n", encoding="utf-8")
    path.chmod(0o600)


def _digest_json(value):
    return hashlib.sha256(json.dumps(value, sort_keys=True, ensure_ascii=False,
                                     separators=(",", ":")).encode()).hexdigest()


def _artifact(path):
    return {"path": str(path), "sha256": sha(path), "bytes": path.stat().st_size}


def artifact_manifest(mnemed: Path, mcp: Path, *, build_inventory: Path | None = None,
                      cache_root: Path | None = None):
    """Reject hashing builds rather than silently changing the study embedder."""
    inventory = BUILD_MANIFEST if build_inventory is None else Path(build_inventory).resolve()
    manifest = json.loads(inventory.read_text())
    source_pin = None
    if manifest.get("schema") == "mneme.routing-native-build.v1":
        need(manifest.get("status") == "built" and manifest.get("features") == "default cozo,fastembed,http"
             and manifest.get("source_pins_unchanged") is True, "not a verified default build cut")
        native = {"features": manifest["features"], "binaries": {
            "mnemed": manifest["binaries"]["client"], "mneme-mcp": manifest["binaries"]["mcp"]}}
    elif "feature_matrix" in manifest:
        need(type(manifest.get("exit")) is int and manifest["exit"] == 0
             and manifest.get("source_stable") is True, "not a successful source-stable build cut")
        matrix = manifest["feature_matrix"]
        need(isinstance(matrix, dict) and all(isinstance(matrix.get(name), list)
             and required <= set(matrix[name]) for name, required in (
                 ("mnemed", {"cozo", "fastembed"}),
                 ("mneme-mcp", {"cozo", "fastembed", "http"}))),
             "not a verified default build cut")
        source_path = inventory.parent / "source-before.json"
        pins = json.loads(source_path.read_text())
        need(isinstance(pins, dict) and bool(pins), "native source inventory missing")
        for relative, digest in pins.items():
            path = PurePosixPath(relative)
            need(str(path) == relative and not path.is_absolute() and ".." not in path.parts
                 and isinstance(digest, str) and re.fullmatch(r"[a-f0-9]{64}", digest),
                 "invalid native source pin")
            source = ROOT / relative
            need(source.is_file() and source.resolve().is_relative_to(ROOT.resolve())
                 and sha(source) == digest, "native build source drift: " + relative)
        source_pin = _artifact(source_path)
        native = {"features": "default cozo,fastembed,http", "binaries": manifest["binaries"]}
    else:
        native = manifest["artifacts"]["default"]
    binaries = {name: _artifact(path) for name, path in (("mnemed", mnemed), ("mneme-mcp", mcp))}
    for name, record in binaries.items():
        need(record["sha256"] == native["binaries"][name]["sha256"],
             name + " does not match the frozen native/default build manifest")
    cache = CACHE if cache_root is None else Path(cache_root).resolve()
    model_root = cache / "models--Xenova--bge-base-en-v1.5"
    revision = (model_root / "refs/main").read_text().strip()
    need(bool(re.fullmatch(r"[a-f0-9]{40}", revision)), "invalid cached BGE revision")
    model_blob = model_root / "snapshots" / revision / "onnx/model.onnx"
    need(model_blob.is_file(), "offline BGE-base model unavailable")
    result = {"build_inventory": _artifact(inventory), "features": native["features"],
            "binaries": binaries, "embedding": {"embedding_id": BGE_ID, "dimension": 768,
                "cache": str(cache), "revision": revision, "model_blob": _artifact(model_blob),
                "rerank": False, "offline": True}}
    if source_pin is not None:
        result["native_source_inventory"] = source_pin
    return result


def fixture_env(root: Path, mnemed: Path, home: Path, *, ordinary=False, cache_root: Path | None = None):
    env = {key: value for key, value in os.environ.items() if not key.startswith("MNEME_")}
    for key in ("OPENAI_API_KEY", "OPENAI_ORG_ID", "OPENAI_PROJECT_ID", "HF_HOME", "PYTHONPATH"):
        env.pop(key, None)
    if ordinary:
        for key in tuple(env):
            if key.startswith(("CODEX_", "OPENAI_", "ANTHROPIC_", "XDG_")):
                env.pop(key, None)
        env["HOME"] = str(home)
    env.update(CODEX_HOME=str(home), MNEME_CLIENT_BINARY=str(mnemed), MNEME_RERANK="0",
               HF_HUB_OFFLINE="1", TRANSFORMERS_OFFLINE="1",
               FASTEMBED_CACHE_DIR=str(CACHE if cache_root is None else Path(cache_root).resolve()),
               GIT_CEILING_DIRECTORIES=str(root), PYTHONDONTWRITEBYTECODE="1")
    return env


def _scene_files(files):
    need(isinstance(files, dict), "scene files must be an object")
    rows = []
    for name, contents in files.items():
        need(isinstance(name, str) and isinstance(contents, str), "scene files require text paths/content")
        path = PurePosixPath(name)
        need(str(path) == name and not path.is_absolute()
             and ".." not in path.parts and "\\" not in name and name != "."
             and not any(part in (".mneme", ".codex", ".git") for part in path.parts)
             and path.name != "AGENTS.md", "unsafe scene file path")
        rows.append((path, contents))
    return rows


def materialize_scene(case, project: Path):
    """Only validated scene files cross this boundary; no host/config metadata."""
    rows = _scene_files(case["scene"]["files"])
    project.mkdir()
    for path, contents in rows:
        target = project / path
        target.parent.mkdir(parents=True, exist_ok=True)
        target.write_text(contents, encoding="utf-8")


def seed_inventory(case, card_lookup):
    """A host-only projection: no gold, case ID, split or family is serialized."""
    cards = [{"key": key, **{name: card_lookup[key][name]
              for name in ("summary", "body", "kind", "thread")}} for key in case["memory_keys"]]
    return {"cards": cards, "edges": case["edges"]}


def _seed(case, card_lookup, host: Path, mnemed: Path, env: dict, *, ordinary=False):
    meta = host / ".mneme"
    meta.mkdir()
    _json(meta / "profile.json", {"schema": "mneme.profile.v1", "mode": "isolated"})
    db = meta / "codex-memory.db"
    cli(mnemed, host, env, "capture", "init", db=db)
    identifiers = {}
    for index, key in enumerate(case["memory_keys"]):
        card = card_lookup[key]
        # Ordinary source refs do not expose fixture author labels.
        source_key = f"note-{index + 1:02d}"
        payload = {"source": {"namespace": "project-notes", "key": source_key,
                              "reference": "notes://project/" + source_key},
                   "summary": card["summary"], "body": card["body"]}
        if card["kind"] == "semantic":
            need(card["thread"] is None, "semantic card cannot carry an episode thread")
            result = cli(mnemed, host, env, "capture", "add", "--input", "-",
                         payload=payload if ordinary else {**payload, "active": True}, db=db)
            identifiers[key] = result["id"]
        else:
            need(card["kind"] == "episode", "unknown memory kind")
            if card["thread"] is not None:
                payload["thread"] = card["thread"]
            # The fixture supplies scope/date in its text, not a fabricated exact
            # occurrence time. Native unknown occurrence remains typed unknown.
            result = cli(mnemed, host, env, "episode", "append", "--input", "-",
                         payload={**payload, "occurred": {"kind": "unknown"}}, db=db)
            identifiers[key] = result["edition_id"]
    for edge in case["edges"]:
        cli(mnemed, host, env, "link", "--from", identifiers[edge["from"]],
            "--to", identifiers[edge["to"]], "--kind", edge["kind"].replace("_", "-"),
            "--weight", str(edge["weight"]), db=db)
    return identifiers


def prepare_template(case, card_lookup, root: Path, *, mnemed: Path, env: dict):
    """Create one immutable, host-private seed for exact three-arm cloning."""
    root = root.resolve()
    need(not root.exists(), "seed template already exists")
    root.mkdir(parents=True, mode=0o700)
    identifiers = _seed(case, card_lookup, root, mnemed, env)
    before = logical_cozo(root / ".mneme/codex-memory.db")
    record = {"schema": "mneme.async-task-seed.v1", "inventory_sha256": _digest_json(seed_inventory(case, card_lookup)),
              "identifiers": identifiers, "logical_kv": before, "mnemed_sha256": sha(mnemed)}
    _json(root / "seed.json", record)
    return record


def _clone_template(template, host, case, card_lookup, mnemed):
    record = json.loads((template / "seed.json").read_text())
    need(record.get("schema") == "mneme.async-task-seed.v1"
         and record["inventory_sha256"] == _digest_json(seed_inventory(case, card_lookup))
         and record["mnemed_sha256"] == sha(mnemed), "seed template identity mismatch")
    source = template / ".mneme/codex-memory.db"
    need(logical_cozo(source) == record["logical_kv"], "seed template changed")
    meta = host / ".mneme"
    meta.mkdir()
    shutil.copyfile(template / ".mneme/profile.json", meta / "profile.json")
    db = meta / "codex-memory.db"
    with closing(sqlite3.connect("file:" + str(source) + "?mode=ro", uri=True)) as src:
        with closing(sqlite3.connect(db)) as dst:
            src.backup(dst)
    bodies = source.with_suffix(".bodies")
    if bodies.exists():
        shutil.copytree(bodies, db.with_suffix(".bodies"))
    need(logical_cozo(db) == record["logical_kv"], "cloned store differs from immutable seed")
    return record


def _body_inventory(root):
    need(root.is_dir() and not root.is_symlink(), "snapshot bodies missing or symlinked")
    rows = []
    for path in sorted(root.rglob("*")):
        need(not path.is_symlink(), "snapshot body symlink")
        if path.is_file():
            rows.append({"path": str(path.relative_to(root)), "bytes": path.stat().st_size,
                         "sha256": sha(path)})
    return rows


def _clone_closed_snapshot(source, manifest_path, host, build_sha256):
    """Exact explicitly pinned closed ordinary source, never init or migration."""
    need(not Path(source).is_symlink(), "snapshot database symlink")
    source, manifest_path = Path(source).resolve(), Path(manifest_path).resolve()
    record = json.loads(manifest_path.read_text())
    need(record.get("schema") == "mneme.ordinary-closed-snapshot.v1"
         and record.get("default_build_sha256") == build_sha256
         and record.get("db", {}).get("path") == str(source), "closed snapshot manifest identity mismatch")
    need(source.is_file() and source.stat().st_nlink == 1, "snapshot database missing or aliased")
    db = host / ".mneme/codex-memory.db"
    need(not db.parent.exists(), "snapshot target must be absent")
    lock_path = source.with_name(source.name + ".mneme.lock")
    need(lock_path.is_file() and not lock_path.is_symlink(), "snapshot existing lease missing")
    with lock_path.open("rb") as lock:
        fcntl.flock(lock, fcntl.LOCK_EX | fcntl.LOCK_NB)
        need(sha(source) == record["db"]["sha256"], "snapshot database changed")
        bodies = source.with_suffix(".bodies")
        need(_body_inventory(bodies) == record.get("bodies"), "snapshot bodies changed or missing")
        before = logical_cozo(source)
        db.parent.mkdir(mode=0o700)
        _json(db.parent / "profile.json", {"schema": "mneme.profile.v1", "mode": "isolated"})
        shutil.copy2(source, db)
        shutil.copytree(bodies, db.with_suffix(".bodies"))
        need(sha(db) == record["db"]["sha256"] and logical_cozo(db) == before
             and _body_inventory(db.with_suffix(".bodies")) == record["bodies"], "snapshot copy differs")
        need(sha(source) == record["db"]["sha256"] and _body_inventory(bodies) == record["bodies"],
             "snapshot source changed during copy")
    return {"manifest": _artifact(manifest_path), "source": str(source),
            "db_sha256": record["db"]["sha256"], "body_files": len(record["bodies"]),
            "logical_kv": before, "mode": "organic_closed_snapshot", "source_unchanged": True}


def _run(argv, *, cwd, env, input=None, timeout=30):
    process = subprocess.Popen([str(x) for x in argv], cwd=cwd, env=env,
        stdin=subprocess.PIPE if input is not None else subprocess.DEVNULL,
        stdout=subprocess.PIPE, stderr=subprocess.PIPE, start_new_session=True)
    try:
        stdout, stderr = process.communicate(input, timeout=timeout)
    except BaseException:
        try:
            os.killpg(process.pid, signal.SIGKILL)
        except ProcessLookupError:
            pass
        process.communicate()
        raise
    need(len(stdout) <= MAX_HELPER_OUTPUT, "fixture helper output exceeded bound")
    need(process.returncode == 0, "fixture helper failed: " + stderr[-800:].decode(errors="replace"))
    return json.loads(stdout) if stdout.strip() else None


# This helper imports only from the installed bundle in a separate interpreter.
# The graph-off diagnostic changes solely the native request depth; it is never
# an implicit fixture warmup and never substitutes for the async worker's pool.
COLLECT_CODE = r'''
import json, sys
from pathlib import Path
import hook_recall, hooks
value = json.load(sys.stdin)
if value['depth'] == 0:
    Original = hook_recall.McpClient
    class GraphOff(Original):
        def call_tool(self, name, arguments):
            if name == 'recall_context':
                assert arguments['depth'] == 2
                arguments = {**arguments, 'depth': 0}
            return super().call_tool(name, arguments)
    hook_recall.McpClient = GraphOff
pool = hook_recall.collect_reader(Path(value['service']), value['cue'], Path(value['project']), timeout=1.5)
result = {'pool': pool}
if value['baseline']:
    selected = pool.get('cards', [])[:2]
    kept = []
    for card in selected:
        # Reserve exactly the async emission prefix budget with maximum valid
        # identity lengths, so baseline never gets a larger rendered-card cap.
        reserve = 'Mneme async reader emission for source session_id=' + 's'*160 + ', turn_id=' + 't'*160 + ' (emitted by this hook, not acknowledged as seen). '
        if hooks._valid_card(card) and len((reserve + hooks._render_cards(kept + [card])).encode()) <= hooks.MAX_CONTEXT_BYTES:
            kept.append(card)
    context = hooks._render_cards(kept)
    result.update(cards=kept, context=context, context_bytes=len(context.encode()),
                  selected_ids=[c['id'] for c in kept], policy='first_two_source_order')
print(json.dumps(result, ensure_ascii=False))
'''


def _processes():
    result = subprocess.run(["ps", "-axo", "pid=,ppid=,pgid=,command="], capture_output=True, text=True, check=True)
    rows = {}
    for line in result.stdout.splitlines():
        fields = line.strip().split(None, 3)
        if len(fields) == 4 and all(x.isdigit() for x in fields[:3]):
            rows[int(fields[0])] = {"ppid": int(fields[1]), "pgid": int(fields[2]), "command": fields[3]}
    return rows


def _owned_tree(rows, prefix):
    marker = str(prefix / "lib/reader_worker.py")
    observer = str(ROOT / "tools/async_task_observer.py")
    owned = {pid for pid, row in rows.items()
             if (marker in row["command"] or
                 (observer in row["command"] and "--bundle-lib " + str(prefix / "lib") in row["command"]
                  and "--serve" in row["command"]))
             and str(prefix / "config/hooks.json") in row["command"]}
    while True:
        more = {pid for pid, row in rows.items() if row["ppid"] in owned}
        if more <= owned:
            return owned
        owned |= more


@dataclass
class Fixture:
    root: Path
    project: Path
    host_project: Path
    home: Path
    env: dict
    prefix: Path
    service_config: Path
    hook_config: Path
    home_hooks: Path
    state_dir: Path
    db: Path
    mnemed: Path
    identifiers: dict = field(default_factory=dict)
    manifest: dict = field(default_factory=dict)
    before: dict | None = None
    after: dict | None = None
    cleanup: dict = field(default_factory=dict)
    service_started: bool = False
    ordinary: bool = False
    task_index: int = 1
    cache_root: Path | None = None

    def next_task(self, files: dict, *, drain: dict):
        """Advance only after the caller's full drain; keep the same native identity.

        Prior task/home are removed after exported evidence; they are not copied
        into the next task. This method launches nothing and never substitutes for drain.
        """
        need(self.ordinary and isinstance(drain, dict) and drain.get("drain_completed") is True
             and drain.get("usage_unknown") is False, "ordinary task requires settled accounting before advance")
        need(not _owned_tree(_processes(), self.prefix), "previous owned reader still alive")
        need(drain.get("recording_mode") == "automatic"
             and isinstance(drain.get("recording_receipt_export"), dict)
             and not drain["recording_receipt_export"].get("errors")
             and drain["recording_receipt_export"].get("truncated") is False,
             "advance requires exported recorder evidence")
        _scene_files(files)  # Refuse malformed input before deleting closed source.
        index = self.task_index + 1
        project = self.host_project / ("task-" + str(index))
        home = self.root / ("codex-home-" + str(index))
        need(not project.exists() and not home.exists(), "next task/home already exists")
        # Only the external explicit auth target survives, never a prior rollout.
        auth = (self.home / "auth.json").resolve()
        hook_bytes = self.home_hooks.read_bytes()
        need(auth.is_file() and not auth.is_relative_to(self.home), "auth must be external private handoff")
        shutil.rmtree(self.project)
        shutil.rmtree(self.home)
        materialize_scene({"scene": {"files": files}}, project)
        home.mkdir(mode=0o700)
        (home / "auth.json").symlink_to(auth)
        (home / "hooks.json").write_bytes(hook_bytes)
        (home / "hooks.json").chmod(0o600)
        self.project, self.home, self.home_hooks = project, home, home / "hooks.json"
        self.task_index = index
        self.env = fixture_env(self.root, self.prefix / "bin/mnemed", home, ordinary=True,
                               cache_root=self.cache_root)
        config = json.loads(self.hook_config.read_text())
        config["reader_auth"] = str(home / "auth.json")
        _json(self.hook_config, config)
        self.manifest["active_home"] = str(home)
        self.manifest["hook_config_sha256"] = sha(self.hook_config)
        return self.project


    @property
    def codex_home(self):
        return self.home

    def __getitem__(self, name):
        return getattr(self, name)

    def _helper(self, code, value=None, timeout=30):
        return _run([sys.executable, "-c", code], cwd=self.prefix / "lib",
            env={**self.env, "PYTHONPATH": str(self.prefix / "lib")},
            input=json.dumps(value).encode() if value is not None else None, timeout=timeout)

    def collect(self, cue, *, depth=2):
        need(depth in (0, 2), "only production depth 2 or explicit graph-off depth 0")
        return self._collect(cue, depth=depth, baseline=False)["pool"]

    def native_baseline(self, cue):
        started = time.monotonic()
        result = self._collect(cue, depth=2, baseline=True)
        result["blocking_elapsed_ms"] = round((time.monotonic() - started) * 1000)
        result["elapsed_ms"] = result["blocking_elapsed_ms"]
        result["result"] = result["pool"]
        return result

    def _collect(self, cue, *, depth, baseline):
        need(self.service_started, "fixture service is not running")
        return self._helper(COLLECT_CODE, {"cue": cue, "service": str(self.service_config),
            "project": str(self.host_project), "depth": depth, "baseline": baseline}, timeout=6)

    def close(self):
        if self.cleanup:
            return
        errors = []
        rows = _processes()
        owned = _owned_tree(rows, self.prefix)
        groups = {rows[pid]["pgid"] for pid in owned if rows[pid]["pgid"] == pid}
        sessions = sorted({match.group(1) for pid in owned
                           if (match := re.search(r"--session-id ([A-Za-z0-9_:.-]{1,160})(?:\s|$)", rows[pid]["command"]))})
        try:
            if sessions:
                self._helper("import json,sys; from pathlib import Path; import hooks,reader_worker; "
                    "v=json.load(sys.stdin); c=hooks._config(Path(v['config'])); "
                    "[reader_worker.end_session(c,s) for s in v['sessions']]; print('{}')",
                    {"config": str(self.hook_config), "sessions": sessions})
                deadline = time.monotonic() + 2
                while time.monotonic() < deadline and owned & set(_processes()):
                    time.sleep(.05)
        except Exception as exc:
            errors.append(type(exc).__name__ + ": worker graceful cleanup failed")
        # ReaderRuntime starts its own process group: kill captured descendants,
        # not only the worker's group, if graceful shutdown did not finish.
        remaining = _processes()
        # Include a reader that started after the first snapshot while the
        # worker was completing its in-flight request.
        newly_owned = _owned_tree(remaining, self.prefix)
        owned |= newly_owned
        groups |= {remaining[pid]["pgid"] for pid in newly_owned if remaining[pid]["pgid"] == pid}
        for pgid in groups:
            if any(pid in remaining and remaining[pid]["pgid"] == pgid for pid in owned):
                try:
                    os.killpg(pgid, signal.SIGKILL)
                except ProcessLookupError:
                    pass
        service_result = None
        if self.service_started:
            try:
                service_result = _run([sys.executable, self.prefix / "lib/service.py", "--config",
                    self.service_config, "stop"], cwd=self.host_project, env=self.env)
            except Exception as exc:
                errors.append(type(exc).__name__ + ": service stop failed")
            self.service_started = False
        service_rows = _processes()
        service_pids = {pid for pid, row in service_rows.items()
                        if str(self.prefix / "bin/mneme-mcp") in row["command"]
                        and "project=" + str(self.db) in row["command"]}
        # A failed wrapper must not strand our disposable native service. Its
        # exact installed binary and DB argv are an ownership check, not a
        # borrowed PID from global service state.
        for pid in service_pids:
            try:
                if service_rows[pid]["pgid"] == pid:
                    os.killpg(pid, signal.SIGKILL)
                else:
                    os.kill(pid, signal.SIGKILL)
            except ProcessLookupError:
                pass
        if service_pids:
            deadline = time.monotonic() + 2
            while time.monotonic() < deadline and service_pids & set(_processes()):
                time.sleep(.05)
        try:
            self.after = logical_cozo(self.db)
        except Exception as exc:
            errors.append(type(exc).__name__ + ": read-only post-state failed")
        remaining = _processes()
        gone = not (owned & set(remaining))
        service_gone = not (service_pids & set(remaining))
        self.cleanup.update(worker_pids=sorted(owned), reader_process_groups=sorted(groups),
            workers_and_readers_gone=gone, service_stop=service_result, service_process_gone=service_gone,
            forced_service_cleanup=bool(service_pids),
            persistent_kv_equal=self.before is not None and self.before == self.after, errors=errors)
        # Opening the native owner after shutdown separately proves lease release.
        if not errors and self.db.exists():
            try:
                self.cleanup["store_lease_reopen"] = isinstance(cli(self.mnemed, self.host_project,
                    self.env, "status", db=self.db), dict)
                self.cleanup["reopen_kv_equal"] = logical_cozo(self.db) == self.after
            except Exception as exc:
                errors.append(type(exc).__name__ + ": native lease reopen failed")
                self.cleanup["store_lease_reopen"] = False
        self.cleanup["mutation_policy"] = "ordinary_recording_allowed" if self.ordinary else "read_only_exact_kv"
        self.cleanup["ok"] = bool(not errors and gone and service_gone
            and self.after is not None and (self.ordinary or self.cleanup["persistent_kv_equal"])
            and isinstance(service_result, dict) and service_result.get("state") == "stopped"
            and self.cleanup.get("store_lease_reopen") and self.cleanup.get("reopen_kv_equal"))


@contextmanager
def fixture(case, card_lookup, root: Path, *, mnemed: Path, mcp: Path,
            codex: Path, auth: Path, seed_template: Path | None = None,
            ordinary: bool = False, build_inventory: Path | None = None,
            closed_snapshot: Path | None = None, snapshot_manifest: Path | None = None,
            reader_model: str = MODEL, librarian_effort: str | None = None,
            cache_root: Path | None = None):
    """Install one fresh actor arm, optionally cloning an exact case seed.

    The caller owns root removal and report persistence. The yielded object keeps
    read-only before/after hashes and cleanup facts available after context exit.
    No task query, warmup query, reader invocation or actor is performed here.
    """
    need(type(ordinary) is bool, "ordinary must be boolean")
    need((closed_snapshot is None) == (snapshot_manifest is None), "snapshot requires path and manifest")
    need(closed_snapshot is None or (ordinary and seed_template is None),
         "closed snapshot is ordinary-only and exclusive with authored seed_template")
    need(not ordinary or (build_inventory is not None and seed_template is None),
         "ordinary recording requires exact default build inventory and no template clone")
    root = root.resolve()
    need(not root.exists() or (root.is_dir() and not any(root.iterdir())), "fixture root must be empty")
    root.mkdir(parents=True, exist_ok=True, mode=0o700)
    for path in (mnemed, mcp, codex, auth):
        need(path.is_absolute() and path.is_file(), "missing absolute fixture prerequisite")
    manifest = artifact_manifest(mnemed, mcp, build_inventory=build_inventory, cache_root=cache_root)
    host, home, prefix = root / "host", root / "codex-home", root / "bundle"
    host.mkdir(mode=0o700)
    home.mkdir(mode=0o700)
    (home / "auth.json").symlink_to(auth)
    env = fixture_env(root, mnemed, home, ordinary=ordinary, cache_root=cache_root)
    obj = Fixture(root, host / "project", host, home, env, prefix,
        prefix / "config/service.json", prefix / "config/hooks.json", home / "hooks.json",
        host / ".mneme/codex-hook-state", host / ".mneme/codex-memory.db", mnemed,
        manifest=manifest, ordinary=ordinary, cache_root=cache_root)
    try:
        materialize_scene(case, obj.project)
        if closed_snapshot is not None:
            record = _clone_closed_snapshot(closed_snapshot, snapshot_manifest, host,
                                            manifest["build_inventory"]["sha256"])
            manifest["seed"] = record
        elif seed_template is None:
            obj.identifiers = _seed(case, card_lookup, host, mnemed, env, ordinary=True) if ordinary else _seed(case, card_lookup, host, mnemed, env)
            manifest["seed"] = {"inventory_sha256": _digest_json(seed_inventory(case, card_lookup)),
                                "exact_template_clone": False, "mode": "authored_seed"}
        else:
            seed_template = seed_template.resolve()
            need(not seed_template.is_relative_to(host), "seed template must remain outside actor host")
            if not seed_template.exists():
                prepare_template(case, card_lookup, seed_template, mnemed=mnemed, env=env)
            record = _clone_template(seed_template, host, case, card_lookup, mnemed)
            obj.identifiers = record["identifiers"].copy()
            manifest["seed"] = {**record, "exact_template_clone": True}
        plan = install.prepare(host, prefix, mnemed, mcp, free_port(), recall_mode="async",
                               reader_model=reader_model, reader_codex=codex,
                               librarian_effort=librarian_effort,
                               recording_mode="automatic" if ordinary else "off")
        need(plan["config_revision"] == install.CONFIG_REVISION, "installer configuration revision mismatch")
        install.apply(plan)
        obj.home_hooks.write_bytes((host / ".codex/hooks.json").read_bytes())
        obj.home_hooks.chmod(0o600)
        (host / ".codex/hooks.json").unlink()
        (host / ".codex/config.toml").unlink()
        (host / ".codex").rmdir()
        installed = {row["destination"]: sha(prefix / row["destination"]) for row in plan["files"]}
        need(all(installed[row["destination"]] == row["sha256"] for row in plan["files"]),
             "installed bundle hash mismatch")
        manifest.update(config_revision=install.CONFIG_REVISION, recording_mode="automatic" if ordinary else "off", plan_sha256=plan["plan_sha256"], installed_files=installed,
                        hook_config_sha256=sha(obj.hook_config), home_hooks_sha256=sha(obj.home_hooks),
                        actor_workspace_files=sorted(case["scene"]["files"]))
        if ordinary:
            config = json.loads(obj.hook_config.read_text())
            config["reader_auth"] = str(home / "auth.json")
            _json(obj.hook_config, config)
            manifest["hook_config_sha256"] = sha(obj.hook_config)
        obj.env["MNEME_CLIENT_BINARY"] = str(prefix / "bin/mnemed")
        start = time.monotonic()
        obj.service_started = True
        readiness = _run([sys.executable, prefix / "lib/service.py", "--config", obj.service_config, "start"],
                         cwd=host, env=obj.env)
        need(readiness["state"] == "ready", "installed service not catalog ready")
        manifest["readiness"] = {"state": "catalog_ready", "elapsed_ms": round((time.monotonic() - start) * 1000),
            "embedder_state": "lazy_not_exercised", "reader_state": "cold_not_started",
            "task_queries_before_actor": 0, "policy": "identical catalog-only readiness in every arm"}
        obj.before = logical_cozo(obj.db)
        if seed_template is not None or closed_snapshot is not None:
            need(obj.before == record["logical_kv"], "service startup changed immutable seed")
        yield obj
    finally:
        obj.close()
