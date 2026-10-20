"""Reviewable named-store defaults and device hooks; never install or start owners.

Inputs are explicit reviewed bindings. Native preparation may acquire and freeze
bounded discovery metadata, but does not read memory content or open databases.
The returned artifact is a proposal: publishing hooks and granting trust remain
separate actions.
"""
import argparse
import hashlib
import json
import os
from pathlib import Path
import sys

from host_platform import hook_command, is_executable
from misc_binding import canonical_path
from registry_runtime import SCHEMA, validate_static
from store_registry import SCHEMA as REGISTRY_SCHEMA, load_registry
from tag_config import prepared_fields


def default_registry(*, global_preferences=None, misc=None, global_core=False):
    """Translate reviewed predecessor policies without granting new authority."""
    stores = []
    if type(global_core) is not bool or global_core and global_preferences is None:
        raise ValueError("core requires an explicit reviewed personal owner binding")
    if global_preferences is not None:
        from target_policy import validate_global_preferences
        recording = False
        if isinstance(global_preferences, dict) and global_preferences.get("schema") == "mneme.codex-hooks.config.v10":
            if (global_preferences.get("memory_mode") != "async"
                    or global_preferences.get("memory_scope", "project") != "project"
                    or global_preferences.get("recording_mode") not in ("off", "automatic")):
                raise ValueError("invalid reviewed global-preference hook policy")
            recording = global_preferences["recording_mode"] == "automatic"
            target = validate_global_preferences(global_preferences.get("global_preferences"))
        else:
            target = validate_global_preferences(global_preferences)
        stores.append({"name": "global", "service_config": target["service_config"],
            "db_alias": "user", "database_path": target["database_path"], "db_id": target["db_id"],
            "purpose": "Explicit cross-project user collaboration preferences. Not project facts or private task details.",
            "when": {"workspace": "any", "profiles": ["default", "private"]},
            "read": {"enabled": True, "core": global_core,
                     "cue": "User cross-project collaboration preferences for working with an assistant."},
            "record": {"enabled": recording, "kinds": ["lesson"], "evidence_kinds": ["user_statement"],
                       "required_tags": ["collaboration-preference"], "allow_associations": False,
                       "max_summary_bytes": 384, "max_body_bytes": 1024},
            "maintenance": {"enabled": False}})
    if misc is not None:
        from misc_config import validate_config
        prior = validate_config(misc)
        target = prior["store_target"]
        recording = prior["recording_mode"] == "automatic"
        stores.append({"name": "misc", "service_config": prior["service_config"], **target,
            "purpose": "Useful source-bound experience from ordinary work outside configured projects; retain workspace provenance.",
            "when": {"workspace": "unconfigured", "excluded_roots": prior["excluded_roots"],
                     "profiles": ["default", "private"]},
            "read": {"enabled": True, "core": False},
            "record": {"enabled": recording, "kinds": ["lesson", "episode", "possibility"],
                       "evidence_kinds": ["user_statement", "assistant_assertion", "tool_call", "tool_result", "memory_delivery"],
                       "allow_associations": True, "max_summary_bytes": 512, "max_body_bytes": 2048},
            "maintenance": {"enabled": recording and prior.get("tag_stewardship", False),
                            "tag_guide_id": prior.get("tag_guide_id")}})
    return {"schema": REGISTRY_SCHEMA, "stores": stores}


def prepare(*, store_registry, state_dir, reader_codex, hooks_program, hook_config_path,
            python=sys.executable, librarian_effort="medium", recording_mode="off", reader_auth=None):
    """Produce exact review/trust source; never register a second project actor."""
    registry_path = canonical_path(store_registry)
    registry = load_registry(registry_path)
    reader = canonical_path(Path(reader_codex).resolve(strict=True))
    program = canonical_path(Path(hooks_program).resolve(strict=True))
    interpreter = canonical_path(Path(python).resolve(strict=True))
    destination = canonical_path(hook_config_path)
    if not is_executable(reader) or not is_executable(interpreter) or not program.is_file():
        raise ValueError("registry preparation requires existing reader, dispatcher and Python")
    if program.name != "hook_launcher.py":
        raise ValueError("device registry hooks require the shipped hook_launcher.py dispatcher")
    config = {"schema": SCHEMA, "store_registry": str(registry_path),
              "state_dir": str(canonical_path(state_dir)), "memory_scope": "registry",
              "memory_mode": "async", "reader_model": "gpt-6.1-sol",
              "librarian_effort": librarian_effort, "recording_mode": recording_mode,
              "reader_codex": str(reader), "reader_codex_sha256": hashlib.sha256(reader.read_bytes()).hexdigest(),
              **prepared_fields(recording_mode, False)}
    if reader_auth is not None:
        config["reader_auth"] = str(canonical_path(reader_auth))
    validate_static(config)
    from hooks import MAX_CONFIG
    if len(json.dumps(config, ensure_ascii=False, indent=2).encode()) > MAX_CONFIG:
        raise ValueError("registry hook configuration exceeds byte limit")
    from install import PROGRAMS, hook_handlers
    argv = [str(interpreter), str(program), "--config", str(destination)]
    sources = Path(__file__).parent
    handlers = hook_handlers(hook_command(argv), async_mode=True,
                recording=recording_mode == "automatic", background_command=hook_command([*argv, "--reader-background"]))
    handlers["SessionStart"].insert(0, {"hooks": [{"type": "command", "command": hook_command([*argv, "--core"]),
                                                "timeout": 15, "additionalContextLimit": 0}]})
    return {"schema": "mneme.codex-registry-prepare.v1", "hook_config": config,
            "hook_config_path": str(destination), "store_registry_sha256": registry.digest,
            "global_hooks": {"hooks": handlers},
            "programs": [{"name": name, "sha256": hashlib.sha256((sources / name).read_bytes()).hexdigest()}
                         for name in PROGRAMS],
            "installation": "not performed", "services": "not contacted",
            "session_policy": "quiesce old workers; start fresh sessions; preserve prior ledgers, receipts and unknown outcomes",
            "dispatch_policy": "only genuinely unconfigured workspaces; project hooks own configured sessions"}



def prepare_native(*, memory_config, native_cli, cwd, state_dir, reader_codex,
                   hooks_program, hook_config_path, python=sys.executable,
                   reader_auth=None, workspace_adapters=None, stable_root=None):
    """Preview one device actor; native selection may freeze discovery metadata.

    Workspace adapters choose only retained ledger locations and reader binaries.
    Store activation, purpose, grants and background policy remain native-owned.
    This does not install hooks, grant trust or start memory owners.
    """
    from registry_runtime import NATIVE_SCHEMA, native_memory_config_path, validate_native_static
    from store_registry import native_select
    path = native_memory_config_path(Path(memory_config).absolute())
    native = canonical_path(Path(native_cli).resolve(strict=True))
    workspace = canonical_path(Path(cwd).resolve(strict=True))
    reader = canonical_path(Path(reader_codex).resolve(strict=True))
    program = canonical_path(Path(hooks_program).resolve(strict=True))
    interpreter = canonical_path(Path(python).resolve(strict=True))
    destination = canonical_path(hook_config_path)
    stable_runtime = None
    if stable_root is not None:
        from stable_hooks import entrypoint_argv
        root = canonical_path(stable_root)
        argv = entrypoint_argv(root / "launch")
        stable_runtime = {"stable_root": str(root), "python": str(interpreter),
                          "hooks_program": str(program), "hook_config_path": str(destination)}
    else:
        argv = [str(interpreter), str(program), "--config", str(destination)]
    # Validate the complete shell-facing request before native selection can
    # acquire discovery metadata. Stable preparation is still only a proposal:
    # the root, launcher and generated hook config need not exist yet.
    command = hook_command(argv)
    background_command = hook_command([*argv, "--reader-background"])
    core_command = hook_command([*argv, "--core"])
    if not all(is_executable(executable) for executable in (native, reader, interpreter)) or not program.is_file():
        raise ValueError("native preparation requires existing native CLI, reader, dispatcher and Python")
    if program.name != "hook_launcher.py":
        raise ValueError("native device hooks require shipped hook_launcher.py")
    selected = native_select(str(path), str(native), str(workspace))
    config = {"schema": NATIVE_SCHEMA, "memory_config": str(path), "native_cli": str(native),
              "state_dir": str(canonical_path(state_dir)), "reader_codex": str(reader),
              "reader_codex_sha256": hashlib.sha256(reader.read_bytes()).hexdigest()}
    if reader_auth is not None:
        config["reader_auth"] = str(canonical_path(reader_auth))
    if workspace_adapters is not None:
        if not isinstance(workspace_adapters, list) or len(workspace_adapters) > 64:
            raise ValueError("workspace adapters must be a bounded list of explicit machinery bindings")
        adapters = []
        for adapter in workspace_adapters:
            if not isinstance(adapter, dict) or set(adapter) != {"root", "state_dir", "reader_codex"}:
                raise ValueError("workspace adapters select only root, state_dir and reader_codex")
            pinned_reader = canonical_path(Path(adapter["reader_codex"]).resolve(strict=True))
            if not is_executable(pinned_reader):
                raise ValueError("workspace adapter reader must be executable")
            adapters.append({"root": str(canonical_path(Path(adapter["root"]).resolve(strict=True))),
                "state_dir": str(canonical_path(adapter["state_dir"])), "reader_codex": str(pinned_reader),
                "reader_codex_sha256": hashlib.sha256(pinned_reader.read_bytes()).hexdigest()})
        config["workspace_adapters"] = sorted(adapters, key=lambda item: item["root"])
    validate_native_static(config)
    from hooks import MAX_CONFIG
    if len(json.dumps(config, ensure_ascii=False, indent=2).encode()) > MAX_CONFIG:
        raise ValueError("native hook adapter exceeds byte limit")
    from install import PROGRAMS, hook_handlers
    # Always install lifecycle/wakeup recipes: per-operation native flags decide
    # work after selection. Core is independent of background enabled/recording.
    # Freeze the existing recipe revision for the stable entrypoint. Runtime
    # package revisions must not become native hook definition changes. The
    # complete stable recipe is also covered by an explicit contract test.
    handler_options = {"revision": 32} if stable_runtime is not None else {}
    handlers = hook_handlers(command, async_mode=True, recording=True,
        background_command=background_command, **handler_options)
    handlers["SessionStart"].insert(0, {"hooks": [{"type": "command",
        "command": core_command, "timeout": 15, "additionalContextLimit": 0}]})
    sources = Path(__file__).parent
    result = {"schema": "mneme.codex-native-prepare.v1", "hook_config": config,
        "hook_config_path": str(destination), "selection": selected.snapshot,
        "native_cli_sha256": hashlib.sha256(native.read_bytes()).hexdigest(),
        "global_hooks": {"hooks": handlers},
        "programs": [{"name": name, "sha256": hashlib.sha256((sources / name).read_bytes()).hexdigest()}
                     for name in PROGRAMS],
        "installation": "not performed", "services": "not contacted",
        "session_policy": "disable old hooks; close old sessions; quiesce all retained ledgers; start fresh sessions",
        "dispatch_policy": "one device actor for all workspaces; no additive project hook actors"}
    if stable_runtime is not None:
        result["stable_runtime"] = stable_runtime
        result["trust_policy"] = (
            "trust the installed Mneme integration; selecting an updated runtime does not change "
            "the native hook definition; initial trust and native recipe changes remain explicit")
    return result


def quiescence(state_dir):
    """Read-only worker snapshot; disabling old hooks is a required precondition.

    Probe existing files only, without creating/removing locks or editing paid
    accounting. This is not a lock held across migration and cannot prevent a
    still-enabled old hook from spawning a later worker.
    """
    import host_files
    import host_process
    import time
    from reader_worker import _valid as valid_ledger
    from hooks import _bounded_json, MAX_STATE, HookError
    root = canonical_path(state_dir)
    directory = root / "reader"
    result = {"schema": "mneme.codex-registry-quiescence.v1", "state_dir": str(root),
              "quiescent_snapshot": False, "coverage": "complete", "active_or_unknown": [],
              "worker_locks_checked": 0, "ledgers_checked": 0,
              "precondition": "disable old hooks and close their sessions before checking; this snapshot is not a migration lock",
              "preserved": "all files, queues, usage, unknown outcomes and receipts"}
    try:
        directory_fd = host_files.open_directory(directory)
    except FileNotFoundError:
        result.update(quiescent_snapshot=True, coverage="no retained reader directory")
        return result
    os.close(directory_fd)
    names, name_bytes = [], 0
    with os.scandir(directory) as entries:
        for entry in entries:
            name_bytes += len(entry.name.encode()) + 1
            if name_bytes > 256 * 1024:
                result["coverage"] = "directory byte limit"
                return result
            names.append(entry.name)
    decoded_bytes = 0
    deadline = time.monotonic() + 10.0
    for name in sorted(names):
        if time.monotonic() >= deadline:
            result["coverage"] = "liveness deadline"
            return result
        path = directory / name
        if name.endswith(".worker.lock") or name.startswith("slot-") and name.endswith(".lock"):
            try:
                fd = host_files.open_regular(path, os.O_RDWR)
                try:
                    host_files.lock_file(fd, blocking=False)
                    result["worker_locks_checked"] += 1
                finally:
                    os.close(fd)
            except (OSError, ValueError):
                result["active_or_unknown"].append({"file": name, "reason": "worker lock held or unavailable"})
        elif name.endswith(".json"):
            try:
                data, raw = _bounded_json(path, MAX_STATE, with_raw=True)
                decoded_bytes += len(raw)
                if decoded_bytes > 8 * 1024 * 1024:
                    result["coverage"] = "ledger byte limit"
                    return result
                if (not valid_ledger(data) or "worker_pid" not in data
                        or data["worker_pid"] is not None and (type(data["worker_pid"]) is not int or data["worker_pid"] <= 0)
                        or data.get("worker_identity") is not None and not isinstance(data["worker_identity"], dict)):
                    raise ValueError("unrecognized worker ledger")
                result["ledgers_checked"] += 1
                pid = data.get("worker_pid")
                expected = data.get("worker_identity")
                if expected is not None and (set(expected) != {"platform", "pid", "created", "executable"}
                        or expected.get("platform") not in ("windows", "posix") or expected.get("pid") != pid
                        or not isinstance(expected.get("executable"), str) or not expected["executable"]
                        or len(expected["executable"].encode()) > 32768
                        or (expected["platform"] == "posix" and (not isinstance(expected.get("created"), str)
                            or len(expected["created"]) != 24))
                        or (expected["platform"] == "windows" and (type(expected.get("created")) is not int
                            or expected["created"] < 0))):
                    raise ValueError("unrecognized worker identity")
                if pid is not None:
                    alive = host_process.process_alive(pid)
                    actual = host_process.process_identity(pid) if alive is not False and expected is not None else None
                    if alive is not False and not (expected is not None and actual is not None and expected != actual):
                        result["active_or_unknown"].append({"file": name, "reason": "worker process active or unknown"})
            except (OSError, ValueError, TypeError, HookError):
                result["active_or_unknown"].append({"file": name, "reason": "worker ledger unavailable or unrecognized"})
    result["quiescent_snapshot"] = not result["active_or_unknown"]
    return result


def main(argv=None):
    parser = argparse.ArgumentParser(description=__doc__)
    sub = parser.add_subparsers(dest="operation", required=True)
    quiet = sub.add_parser("quiescence", help="check retained worker liveness without changing accounting")
    quiet.add_argument("--state-dir", required=True)
    defaults = sub.add_parser("defaults", help="preview entries from explicitly reviewed predecessor bindings")
    defaults.add_argument("--global-preferences", type=Path)
    defaults.add_argument("--misc-config", type=Path)
    defaults.add_argument("--global-core", action="store_true",
                          help="explicitly grant startup core to the reviewed personal owner")
    prep = sub.add_parser("prepare", help="preview device dispatcher config and exact hook definitions")
    for name in ("store-registry", "state-dir", "reader-codex", "hooks-program", "hook-config-path"):
        prep.add_argument("--" + name, required=True)
    prep.add_argument("--python", default=sys.executable)
    prep.add_argument("--recording-mode", choices=("off", "automatic"), default="off")
    prep.add_argument("--librarian-effort", choices=("low", "medium", "high"), default="medium")
    prep.add_argument("--reader-auth")
    native = sub.add_parser("prepare-native", help="preview one device actor driven by native memory.toml")
    for name in ("memory-config", "native-cli", "cwd", "state-dir", "reader-codex", "hooks-program", "hook-config-path"):
        native.add_argument("--" + name, required=True)
    native.add_argument("--python", default=sys.executable)
    native.add_argument("--reader-auth")
    native.add_argument("--workspace-adapters", type=Path,
                        help="reviewed JSON list of root/state_dir/reader_codex machinery bindings, not policy")
    native.add_argument("--stable-root", type=Path,
                        help="opt into a fixed POSIX hook entrypoint; stage/select its runtime with stable_hooks.py")
    args = vars(parser.parse_args(argv))
    operation = args.pop("operation")
    if operation == "quiescence":
        value = quiescence(args["state_dir"])
    elif operation == "defaults":
        from hooks import _bounded_json, MAX_CONFIG
        value = default_registry(global_preferences=_bounded_json(args["global_preferences"], MAX_CONFIG)
                                 if args["global_preferences"] else None,
                                 misc=_bounded_json(args["misc_config"], MAX_CONFIG) if args["misc_config"] else None,
                                 global_core=args["global_core"])
    elif operation == "prepare-native":
        from hooks import _bounded_json, MAX_CONFIG
        if args["workspace_adapters"] is not None:
            args["workspace_adapters"] = _bounded_json(args["workspace_adapters"], MAX_CONFIG)
        value = prepare_native(**args)
    else:
        value = prepare(**args)
    print(json.dumps(value, ensure_ascii=True, indent=2))
    return 0 if operation != "quiescence" or value["quiescent_snapshot"] else 1


if __name__ == "__main__":
    raise SystemExit(main())
