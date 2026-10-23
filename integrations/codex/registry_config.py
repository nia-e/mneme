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



ACTIVITY_SCHEMA = "mneme.session-activity.config.v1"
ACTIVITY_PREPARE_SCHEMA = "mneme.codex-activity-prepare.v1"
MAX_ACTIVITY_BYTES = 64 * 1024
ACTIVITY_SERVER = "mneme_session_activity"


def _activity_bytes(value):
    return (json.dumps(value, ensure_ascii=False, sort_keys=True, separators=(",", ":"),
                       allow_nan=False) + "\n").encode()


def _activity_path(value, *, existing=False):
    if (not isinstance(value, (str, Path)) or len(str(value).encode()) > 4096
            or any(ord(c) < 32 or 127 <= ord(c) <= 159 for c in str(value))):
        raise ValueError("activity paths must be bounded canonical absolute paths")
    path = canonical_path(value, directory=existing)
    if str(path) != str(value) or len(path.parts) > 256:
        raise ValueError("activity paths must be canonical absolute paths")
    return path


def _activity_manifest(value, *, filesystem=True):
    if (not isinstance(value, dict) or set(value) - {"schema", "root", "workspaces", "device"}
            or not {"schema", "root", "workspaces"} <= set(value)
            or value["schema"] != ACTIVITY_SCHEMA):
        raise ValueError("unsupported activity manifest")
    paths = [value["root"]]
    workspaces = value["workspaces"]
    if not isinstance(workspaces, list) or not 1 <= len(workspaces) <= 256:
        raise ValueError("activity workspaces must be a bounded nonempty list")
    seen = set()
    for binding in workspaces:
        if (not isinstance(binding, dict) or set(binding) != {"path", "source"}
                or binding["source"] != "codex" or not isinstance(binding["path"], str)
                or binding["path"] in seen):
            raise ValueError("invalid or duplicate activity workspace binding")
        seen.add(binding["path"])
        paths.append(binding["path"])
    for index, path in enumerate(paths):
        if (not isinstance(path, str) or len(path.encode()) > 4096 or "\x00" in path
                or not Path(path).is_absolute() or str(Path(path)) != path or ".." in Path(path).parts
                or len(Path(path).parts) > 256
                or any(ord(c) < 32 or 127 <= ord(c) <= 159 for c in path)):
            raise ValueError("invalid activity path")
        if filesystem and index == 0:
            _activity_path(path)
        # Retained, unrelated bindings are historical metadata. Their workspace
        # may have moved or disappeared; only the current/new binding is probed.
    device = value.get("device")
    if (device is not None and (not isinstance(device, str) or not device or len(device.encode()) > 128
            or any(ord(c) < 32 or 127 <= ord(c) <= 159 for c in device))):
        raise ValueError("activity device must be a bounded label or null")
    if len(_activity_bytes(value)) > MAX_ACTIVITY_BYTES:
        raise ValueError("activity manifest exceeds 64 KiB")
    return value


def _read_activity(path):
    from host_files import check_private_fd, open_regular
    _activity_path(path)
    try:
        descriptor = open_regular(path)
    except FileNotFoundError:
        return None
    with os.fdopen(descriptor, "rb") as stream:
        check_private_fd(stream)
        raw = stream.read(MAX_ACTIVITY_BYTES + 1)
    if len(raw) > MAX_ACTIVITY_BYTES:
        raise ValueError("activity manifest exceeds 64 KiB")
    # Reuse the producer's bounded, duplicate-key/depth rejecting decoder only;
    # no operational helper, lock or retention path runs during preparation.
    from session_activity import _decode
    _activity_manifest(_decode(raw))
    return raw


def prepare_activity(*, cwd, activity_root, activity_mcp, activity_config=None, activity_device=None):
    """Prepare a shared diagnostic binding; no directory creation or publication."""
    from host_files import check_private_directory
    from host_platform import default_config_dir
    if os.name != "posix":
        raise ValueError("platform_unsupported: session activity setup requires Unix")
    workspace = _activity_path(cwd, existing=True)
    root = _activity_path(activity_root)
    destination = _activity_path(activity_config or default_config_dir() / "activity.json")
    executable = _activity_path(activity_mcp)
    if root.exists():
        check_private_directory(root)
    before = _read_activity(destination)
    manifest = {"schema": ACTIVITY_SCHEMA, "root": str(root), "workspaces": [], "device": activity_device}
    if before is not None:
        from session_activity import _decode
        manifest = _decode(before)
        if manifest["root"] != str(root):
            raise ValueError("activity root differs from existing binding; refusing replacement")
        if activity_device is not None and manifest.get("device") != activity_device:
            raise ValueError("activity device differs from existing binding; review it separately")
        manifest.setdefault("device", None)
    binding = {"path": str(workspace), "source": "codex"}
    if binding not in manifest["workspaces"]:
        manifest["workspaces"].append(binding)
    _activity_manifest(manifest)
    return {"schema": ACTIVITY_PREPARE_SCHEMA, "config_path": str(destination),
            "expected_sha256": None if before is None else hashlib.sha256(before).hexdigest(),
            "manifest": manifest,
            "mcp": {"command": str(executable),
                    "args": ["--activity-config", str(destination), "--cwd", str(workspace)],
                    "enabled_tools": ["session_activity"]}}


def validate_activity_preparation(value, *, cwd, activity_mcp):
    """Static receipt validation, including uninstall after retained paths disappear."""
    if (not isinstance(value, dict) or set(value) != {"schema", "config_path", "expected_sha256", "manifest", "mcp"}
            or value["schema"] != ACTIVITY_PREPARE_SCHEMA):
        raise ValueError("unsupported prepared activity binding")
    _activity_manifest(value["manifest"], filesystem=False)
    path = value["config_path"]
    if (not isinstance(path, str) or not Path(path).is_absolute() or str(Path(path)) != path
            or ".." in Path(path).parts or len(path.encode()) > 4096 or len(Path(path).parts) > 256
            or any(ord(c) < 32 or 127 <= ord(c) <= 159 for c in path)):
        raise ValueError("invalid activity config path")
    fingerprint = value["expected_sha256"]
    if fingerprint is not None and (not isinstance(fingerprint, str) or len(fingerprint) != 64
                                   or any(c not in "0123456789abcdef" for c in fingerprint)):
        raise ValueError("invalid activity config fingerprint")
    if {"path": str(cwd), "source": "codex"} not in value["manifest"]["workspaces"]:
        raise ValueError("prepared activity workspace does not match project")
    if value["mcp"] != {"command": str(activity_mcp),
            "args": ["--activity-config", path, "--cwd", str(cwd)], "enabled_tools": ["session_activity"]}:
        raise ValueError("prepared activity MCP does not match project")


def revalidate_activity(value):
    """Fence changed inputs before installer output or setup directory creation."""
    from host_files import check_private_directory
    manifest = _activity_manifest(value["manifest"])
    _activity_path(value["mcp"]["args"][-1], existing=True)
    root = _activity_path(manifest["root"])
    if root.exists():
        check_private_directory(root)
    before = _read_activity(_activity_path(value["config_path"]))
    desired = _activity_bytes(manifest)
    if before != desired and (None if before is None else hashlib.sha256(before).hexdigest()) != value["expected_sha256"]:
        raise ValueError("activity manifest changed since prepare; prepare a new plan")


def _create_activity_directory(path, *, private):
    from host_files import ensure_private_directory, ensure_state_directory, sync_directory
    create = ensure_private_directory if private else ensure_state_directory
    create(path)
    # Include ancestors on replay too: a previous setup may have created them
    # before losing its durability acknowledgement. Paths are bounded to 256 parts.
    # Existing config directory modes stay intact; these are only sync barriers.
    for directory in (path, *path.parents):
        sync_directory(directory)


def publish_activity(value):
    """Explicit setup only; preserve shared enrollment/data on later install failure.

    A small independent setup lock fences concurrent manifest merges. Replaying
    the exact published bytes is safe after a lost fsync acknowledgement; unknown
    bytes always require a new prepare. No reader uses or creates this lock.
    """
    from host_files import (atomic_write, check_private_fd, lock_file, open_regular,
                            sync_directory, sync_file)
    revalidate_activity(value)
    destination = _activity_path(value["config_path"])
    manifest = _activity_manifest(value["manifest"])
    root = _activity_path(manifest["root"])
    desired = _activity_bytes(manifest)
    # Refuse drift before creating the setup directory or diagnostic root.
    before = _read_activity(destination)
    if before != desired and (None if before is None else hashlib.sha256(before).hexdigest()) != value["expected_sha256"]:
        raise ValueError("activity manifest changed since prepare; prepare a new plan")
    _create_activity_directory(destination.parent, private=False)
    lock = open_regular(destination.with_name("." + destination.name + ".setup.lock"), os.O_RDWR | os.O_CREAT)
    try:
        check_private_fd(lock)
        lock_file(lock, blocking=False)
        before = _read_activity(destination)
        if before != desired and (None if before is None else hashlib.sha256(before).hexdigest()) != value["expected_sha256"]:
            raise ValueError("activity manifest changed since prepare; prepare a new plan")
        _create_activity_directory(root, private=True)
        if before != desired:
            atomic_write(destination, desired)
        else:
            # A previous rename may have succeeded before its fsync failed.
            # Matching bytes prove identity, not durability of that publication.
            sync_file(destination)
            sync_directory(destination.parent)
        if _read_activity(destination) != desired:
            raise ValueError("activity publication readback differs; retain binding and prepare a new plan")
    finally:
        os.close(lock)


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
                   reader_auth=None, workspace_adapters=None, stable_root=None,
                   activity_root=None, activity_config=None, activity_device=None, activity_mcp=None):
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
    activity = None
    if activity_root is not None:
        if activity_mcp is None:
            raise ValueError("--activity-root requires an explicit --activity-mcp executable")
        activity = prepare_activity(cwd=workspace, activity_root=activity_root,
            activity_mcp=activity_mcp, activity_config=activity_config, activity_device=activity_device)
        if not is_executable(activity["mcp"]["command"]):
            raise ValueError("activity MCP must be an existing executable")
    elif any(value is not None for value in (activity_config, activity_device, activity_mcp)):
        raise ValueError("activity options require explicit --activity-root")
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
    if activity is not None:
        result["activity"] = activity
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
    native.add_argument("--activity-root", type=Path,
                        help="opt in to a separately reviewed private diagnostic root and manifest proposal")
    native.add_argument("--activity-config", type=Path,
                        help="manifest override; default platform Mneme config directory/activity.json")
    native.add_argument("--activity-device", help="optional device display label")
    native.add_argument("--activity-mcp", type=Path,
                        help="explicit MCP executable for separate stdio-only activity entry")
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
