#!/usr/bin/env python3
"""Prepare/apply a reviewable project-only Codex configuration, without trusting hooks.

This installer copies explicitly supplied binaries; it does not build/download,
initialize a database, start a daemon, modify global Codex config, or grant hook
trust. Explicit --activity-root additionally enrolls a shared diagnostic binding.
Uninstall restores only unchanged owned Codex outputs and keeps diagnostic data/bindings.
"""
from __future__ import annotations

import argparse
import hashlib
import json
import os
from pathlib import Path
import re
import shlex
import sys
try:
    import tomllib
except ImportError:
    raise SystemExit("Mneme installer requires Python 3.11 or newer (for TOML parsing).")

SCHEMA = "mneme.codex-install.v1"
HOST_PLATFORM = "windows" if os.name == "nt" else "posix"
CONFIG_REVISION = 44
EXISTING_SERVICE_REVISION = 45
EXISTING_SERVICE_REVISIONS = (21, 23, 25, 27, 29, 31, 33, 35, 37, 39, 41, 43, EXISTING_SERVICE_REVISION)
SUPPORTED_APPLY_REVISIONS = tuple(range(20, EXISTING_SERVICE_REVISION + 1))
SERVER = "mneme_project"
BEGIN = "# BEGIN mneme-codex-v1"
END = "# END mneme-codex-v1"
PROGRAMS = ("hooks.py", "mcp_client.py", "service.py", "memory.py", "launcher.py", "hook_recall.py", "profile.py", "library_launcher.py", "shadow.py", "reader_worker.py", "reader_runtime.py", "reader_contract.py", "turn_observer.py", "rollout_primitives.py", "recording_contract.py", "recording_jobs.py", "routing_memory.py", "routing_contract.py", "librarian_policy.py", "touchstone_contract.py", "target_policy.py", "misc_binding.py", "misc_config.py", "hook_launcher.py", "tag_config.py", "tag_context.py", "stewardship.py", "stewardship_contract.py", "host_files.py", "host_process.py", "host_platform.py", "store_registry.py", "registry_config.py", "registry_runtime.py", "registry_reader.py", "registry_recording.py", "core_hook.py", "guidance.py", "touchstone_handoff.py", "assessment.py", "capture_source.py", "claude_observer.py", "claude_runtime.py", "claude_config.py", "claude_hooks.py", "review_contract.py", "stable_hooks.py", "session_activity.py", "packet_accounting.py")
LEGACY_TOOLS = ["recall_context", "get", "capture", "supersede", "status"]
EPISODE_TOOLS = [*LEGACY_TOOLS, "episode"]
SAVE_TOOLS = [*EPISODE_TOOLS, "save"]
TOOLS = [*SAVE_TOOLS, "concern"]
NATIVE_TOOLS = ["core", "recall_context", "get", "list", "neighbors", "save", "episode", "status"]
MAX_CONFIG_BYTES = 1024 * 1024
# ConfigSource witnesses use mneme-config's per-file and aggregate input bounds.
MAX_NATIVE_SOURCE_BYTES = 64 * 1024
MAX_NATIVE_SOURCE_TOTAL_BYTES = 4 * MAX_NATIVE_SOURCE_BYTES
CONFIG_NAMES = ("config.toml", "hooks.json")
LEGACY_DESTINATIONS = (("bin/mnemed", True), ("bin/mneme-mcp", True),
                       *(("lib/" + name, False) for name in PROGRAMS[:4]))
V3_DESTINATIONS = (*LEGACY_DESTINATIONS, ("lib/launcher.py", False))
V4_DESTINATIONS = (*V3_DESTINATIONS, ("lib/hook_recall.py", False))
V5_DESTINATIONS = (*V4_DESTINATIONS, ("lib/profile.py", False))
V7_DESTINATIONS = (*V5_DESTINATIONS, ("lib/library_launcher.py", False))
V8_DESTINATIONS = (*V7_DESTINATIONS, ("lib/shadow.py", False))
V9_DESTINATIONS = (*V8_DESTINATIONS, *(("lib/" + name, False) for name in PROGRAMS[9:12]))
V10_DESTINATIONS = (*V9_DESTINATIONS, *(("lib/" + name, False) for name in PROGRAMS[12:16]))
V13_DESTINATIONS = (*V10_DESTINATIONS, ("lib/routing_memory.py", False), ("lib/routing_contract.py", False))
V14_DESTINATIONS = (*V13_DESTINATIONS, ("lib/librarian_policy.py", False))
V16_DESTINATIONS = (*V14_DESTINATIONS, ("lib/touchstone_contract.py", False))
V19_DESTINATIONS = (*V16_DESTINATIONS, ("lib/target_policy.py", False))
V20_DESTINATIONS = (*V19_DESTINATIONS, *(("lib/" + name, False) for name in PROGRAMS[21:24]))
V22_DESTINATIONS = (*V20_DESTINATIONS, *(("lib/" + name, False) for name in PROGRAMS[24:28]))
V24_DESTINATIONS = (*V22_DESTINATIONS, *(("lib/" + name, False) for name in PROGRAMS[28:37]))
V26_DESTINATIONS = (*V24_DESTINATIONS, ("lib/guidance.py", False))
V28_DESTINATIONS = (*V26_DESTINATIONS, ("lib/touchstone_handoff.py", False))
V30_DESTINATIONS = (*V28_DESTINATIONS, *(("lib/" + name, False) for name in PROGRAMS[39:45]))
V32_DESTINATIONS = (*V30_DESTINATIONS, ("lib/review_contract.py", False))
V34_DESTINATIONS = (*V32_DESTINATIONS, ("lib/stable_hooks.py", False))
V36_DESTINATIONS = (*V34_DESTINATIONS, ("lib/session_activity.py", False))
DESTINATIONS = (*V36_DESTINATIONS, ("lib/packet_accounting.py", False))
RECALL_MODES = ("reminder", "automatic", "shadow", "async")
RECORDING_MODES = ("off", "automatic")
SHADOW_MODELS = ("gpt-5.6-sol", "gpt-5.6-terra")
LEGACY_READER_MODELS = ("gpt-5.6-sol",)
from host_files import atomic_write, ensure_private_directory, open_directory, open_regular
from host_platform import hook_command, is_executable, prepare_reader_codex, validate_reader_codex, validate_command, python_hook_argv, prepare_python_selector
from librarian_policy import MODEL, EFFORTS, resolve as resolve_policy
READER_MODELS = (MODEL,)
HEX_SHA256 = re.compile(r"[0-9a-f]{64}\Z")


def digest(data):
    return hashlib.sha256(data).hexdigest()


def read_optional(path):
    if path.is_symlink():
        raise ValueError(f"refusing a symlink config target: {path}")
    try:
        with os.fdopen(open_regular(path), "rb") as f:
            value = f.read(MAX_CONFIG_BYTES + 1)
    except FileNotFoundError:
        return None
    if len(value) > MAX_CONFIG_BYTES:
        raise ValueError(f"config exceeds 1 MiB: {path}")
    return value


def fingerprint(path):
    data = read_optional(path)
    return None if data is None else digest(data)


def private_write(path, data):
    atomic_write(path, data)


def config_directory(path):
    """Create privately; preserve permissions of an existing real directory."""
    try:
        ensure_private_directory(path, parents=False, exist_ok=False)
    except FileExistsError:
        os.close(open_directory(path))


def plan_platform(plan):
    # Original receipts are POSIX artifacts even when read on a newer host.
    return plan.get("host_platform", "posix") if plan.get("config_revision", 1) >= 24 else "posix"


def binary_destination(plan, name):
    return "bin/" + name + (".exe" if plan_platform(plan) == "windows" else "")


def current_destinations(platform):
    return tuple((name + ".exe" if executable and platform == "windows" else name, executable)
                 for name, executable in DESTINATIONS)


def json_bytes(value):
    return (json.dumps(value, indent=2, ensure_ascii=False) + "\n").encode()


def plan_hash(plan):
    body = {key: value for key, value in plan.items() if key != "plan_sha256"}
    return digest(json.dumps(body, sort_keys=True, separators=(",", ":")).encode())


def global_preferences_input(path):
    from target_policy import validate_global_preferences
    return validate_global_preferences(json.loads(read_optional(path)))


def store_registry_input(path):
    """Validate a bounded local registry without contacting or opening its owners."""
    from store_registry import load_registry
    path = Path(path)
    if not path.is_absolute() or path.resolve() != path:
        raise ValueError("store registry must be a canonical absolute regular file")
    registry = load_registry(path)
    # The strict loader validates and hashes one bounded byte snapshot. Service
    # files remain runtime-selection inputs, not ambient installation authority.
    return str(path), registry.digest


def revalidate_store_registry(plan):
    """Fence reviewed registry bytes before installation or native trust writes."""
    if "store_registry" not in plan:
        return
    path, fingerprint = store_registry_input(plan["store_registry"])
    if path != plan["store_registry"] or fingerprint != plan["store_registry_sha256"]:
        raise ValueError("store registry changed since prepare; prepare a new plan")


def memory_config_input(path, native_cli, cwd, snapshot=None, *, expected_selection_digest=None):
    """One native bounded selection; never interpret TOML policy in Python."""
    from registry_runtime import native_memory_config_path
    from store_registry import native_select
    path = native_memory_config_path(path)
    if snapshot is None:
        options = {"expected_selection_digest": expected_selection_digest} if expected_selection_digest is not None else {}
        snapshot = native_select(str(path), str(native_cli), str(cwd), **options).snapshot
    return {"path": str(path), "digest": snapshot["digest"], "sources": snapshot["sources"]}


def revalidate_memory_config(plan):
    if "memory_config" not in plan:
        return
    native = next(row for row in plan["files"] if row["destination"] == binary_destination(plan, "mnemed"))
    pin = plan["memory_config"]
    # Every reviewed native selection replays offline. Even a previously local
    # config must not acquire newly enabled discovery before drift is rejected.
    if memory_config_input(pin["path"], native["source"], plan["project_root"],
                           expected_selection_digest=pin["digest"]) != pin:
        raise ValueError("memory config changed since prepare; prepare a new plan")


def native_mcp_input(memory_config, root):
    """Identify only the supported native registration; never copy user secrets.

    A same-name project table overrides launch machinery and inherits the user's
    tool filters, approvals, environment and disabled state through Codex layers.
    The global file is a guarded input, never an installer output.
    """
    home = os.environ.get("CODEX_HOME")
    if home is None:
        home = Path.home() / ".codex"
    path = Path(home).absolute() / "config.toml"
    raw = read_optional(path)
    parsed = {} if raw is None else tomllib.loads(raw.decode())
    servers = parsed.get("mcp_servers", {})
    if not isinstance(servers, dict):
        raise ValueError("user MCP configuration must be a table")
    matches = []
    for name, server in servers.items():
        if not isinstance(server, dict):
            raise ValueError("user MCP server must be a table")
        command, args = server.get("command"), server.get("args")
        if not isinstance(args, list) or str(memory_config) not in args:
            continue
        if not isinstance(command, str) or Path(command).name not in ("mneme-mcp", "mneme-mcp.exe"):
            raise ValueError("native MCP registration uses unsupported launch machinery; preserve it and configure startup explicitly")
        if args != ["--memory-config", str(memory_config)] or server.get("cwd") is not None:
            raise ValueError("native MCP registration has unsupported workspace/arguments; no override attempted")
        if not re.fullmatch(r"[A-Za-z0-9_.-]{1,128}", name):
            raise ValueError("native MCP registration has an unsupported server name")
        if server.get("enabled", True) is not True:
            raise ValueError("native MCP registration is explicitly disabled; init preserves that choice")
        validate_command(command, default="mneme-mcp")
        matches.append((name, command))
    if len(matches) > 1:
        raise ValueError("multiple native MCP registrations select this configuration; review them instead of guessing")
    return {"name": matches[0][0] if matches else SERVER,
            "command": matches[0][1] if matches else "mneme-mcp",
            "config_path": str(path), "sha256": None if raw is None else digest(raw),
            "inherited": bool(matches)}


def validate_native_managed(plan):
    if "native_managed" not in plan:
        return
    managed = plan.get("native_managed")
    if (plan.get("config_revision", 1) < 42 or "memory_config" not in plan
            or not isinstance(managed, dict) or set(managed) != {"db_id", "mcp"}
            or not isinstance(managed["db_id"], str)
            or not re.fullmatch(r"[0-7][0-9A-HJKMNP-TV-Z]{25}", managed["db_id"])):
        raise ValueError("invalid native managed project binding")
    mcp = managed["mcp"]
    if (not isinstance(mcp, dict) or set(mcp) != {"name", "command", "config_path", "sha256", "inherited"}
            or not isinstance(mcp["name"], str) or not re.fullmatch(r"[A-Za-z0-9_.-]{1,128}", mcp["name"])
            or not isinstance(mcp["config_path"], str) or not Path(mcp["config_path"]).is_absolute()
            or type(mcp["inherited"]) is not bool
            or (mcp["sha256"] is not None and
                (not isinstance(mcp["sha256"], str) or not HEX_SHA256.fullmatch(mcp["sha256"])))):
        raise ValueError("invalid inherited native MCP binding")
    validate_command(mcp["command"], default="mneme-mcp")


def revalidate_native_managed(plan):
    validate_native_managed(plan)
    if "native_managed" in plan:
        expected = plan["native_managed"]["mcp"]
        actual = native_mcp_input(plan["memory_config"]["path"], Path(plan["project_root"]))
        if actual != expected:
            raise ValueError("user native MCP configuration changed since prepare; prepare a new plan")


def existing_service_input(path):
    """One bounded, regular-file managed config read; never probe its owner."""
    from service import MAX_CONFIG_BYTES as limit, _unique_config_fields
    path = Path(path)
    if not path.is_absolute() or path.resolve() != path:
        raise ValueError("existing service config must be a canonical absolute regular file")
    fd = open_regular(path)
    with os.fdopen(fd, "rb") as stream:
        raw = stream.read(limit + 1)
    if len(raw) > limit:
        raise ValueError("existing service config exceeds 32768 bytes")
    data = json.loads(raw, object_pairs_hook=_unique_config_fields)
    return {"source": str(path), "sha256": digest(raw), "config": data}


def validate_existing_service(plan):
    """Validate reviewed identity only; uninstall must not depend on live inputs."""
    from service import ServiceConfig
    pin = plan.get("existing_service_config")
    if (not isinstance(pin, dict) or set(pin) != {"source", "sha256", "config"}
            or not isinstance(pin["source"], str)
            or not Path(pin["source"]).is_absolute()
            or str(Path(pin["source"])) != pin["source"]
            or not isinstance(pin["sha256"], str) or not HEX_SHA256.fullmatch(pin["sha256"])
            or not isinstance(pin["config"], dict)):
        raise ValueError("invalid existing service config pin")
    data = pin["config"]
    config = ServiceConfig._from_data(data)
    dynamic = plan.get("config_revision", 1) >= 40
    if dynamic:
        validate_command(data["binary"], default="mneme-mcp")
    for name in ((() if dynamic else ("binary",)) + ("state_dir", "working_directory",
                 "project_db" if "project_db" in data else "database_path")):
        value = data[name]
        if (not isinstance(value, str) or str(Path(value)) != value
                or "\x00" in value or not Path(value).is_absolute()
                or value.startswith("//") or ".." in Path(value).parts):
            raise ValueError("existing service paths must be normalized absolute strings")
    if (config.database_name != "project" or str(config.database_path) != plan["project_db"]
            or str(config.working_directory) != plan["project_root"] or config.port != plan["port"]):
        raise ValueError("existing service does not match the exact project, database and port")
    native = next(row for row in plan["files"] if row["destination"] == binary_destination(plan, "mneme-mcp"))
    if not dynamic and "memory_config" not in plan and str(config.binary) != native["source"]:
        raise ValueError("existing service binary must be the selected MCP source")
    if dynamic and "existing_owner_binary" in plan:
        raise ValueError("dynamic service selectors must not contain an implicit owner binary pin")
    if not dynamic and "memory_config" in plan:
        owner = plan.get("existing_owner_binary")
        if (not isinstance(owner, dict) or set(owner) != {"source", "sha256"}
                or owner["source"] != str(config.binary)
                or not isinstance(owner["sha256"], str) or not HEX_SHA256.fullmatch(owner["sha256"])):
            raise ValueError("native install requires a separately pinned retained owner binary")
    return pin


def revalidate_existing_service(plan):
    """Fence owner/config drift before installer or native trust writes."""
    if "existing_service_config" not in plan:
        return
    pin = validate_existing_service(plan)
    if existing_service_input(pin["source"]) != pin:
        raise ValueError("existing service config changed; prepare a new plan")
    if plan.get("config_revision", 1) >= 40:
        return
    native = next(row for row in plan["files"] if row["destination"] == binary_destination(plan, "mneme-mcp"))
    native = plan.get("existing_owner_binary", native)
    binary = Path(native["source"])
    if (binary.resolve() != binary or not is_executable(binary)
            or digest(binary.read_bytes()) != native["sha256"]):
        raise ValueError("existing service binary changed; prepare a new plan")


def validate_plan(plan):
    """Bind reviewed prepare output; this is an accident guard, not authentication."""
    if not isinstance(plan, dict) or plan.get("schema") != SCHEMA:
        raise ValueError("unsupported install plan")
    if not isinstance(plan.get("plan_sha256"), str) or plan_hash(plan) != plan["plan_sha256"]:
        raise ValueError("install plan changed since prepare; prepare a new plan")
    revision = plan.get("config_revision", 1)
    if type(revision) is not int or revision not in range(1, EXISTING_SERVICE_REVISION + 1):
        raise ValueError("unsupported planned configuration revision")
    if revision in EXISTING_SERVICE_REVISIONS:
        if "existing_service_config" not in plan:
            raise ValueError("existing-service revision requires existing service config pin")
    elif "existing_service_config" in plan:
        raise ValueError("existing service config requires an existing-service revision")
    if revision >= 4:
        modes = (RECALL_MODES if revision >= 9 else
                 RECALL_MODES[:3] if revision >= 8 else RECALL_MODES[:2])
        if type(plan.get("recall_mode")) is not str or plan["recall_mode"] not in modes:
            raise ValueError("invalid planned recall mode")
    elif "recall_mode" in plan:
        raise ValueError("old install plan cannot contain recall mode; prepare a new plan")
    if revision >= 5:
        enrollment = plan.get("library_enrollment")
        helper = plan.get("library_helper")
        if enrollment not in ("none", "fresh", "existing"):
            raise ValueError("invalid library enrollment mode")
        if enrollment == "none":
            if helper is not None:
                raise ValueError("unused library helper")
        elif (not isinstance(helper, dict) or set(helper) != {"source", "sha256"}
              or not isinstance(helper["source"], str)
              or not Path(helper["source"]).is_absolute()
              or not isinstance(helper["sha256"], str)
              or not HEX_SHA256.fullmatch(helper["sha256"])):
            raise ValueError("invalid library helper pin")
    if revision >= 8:
        if "shadow_model" not in plan or "shadow_codex" not in plan:
            raise ValueError("planned shadow options are incomplete")
        shadow_model = plan.get("shadow_model")
        shadow_codex = plan.get("shadow_codex")
        if plan["recall_mode"] == "shadow":
            if type(shadow_model) is not str or shadow_model not in SHADOW_MODELS:
                raise ValueError("shadow mode requires an explicit supported shadow model")
            planned_reader_config(shadow_codex)
        elif shadow_model is not None or shadow_codex is not None:
            raise ValueError("shadow options require shadow recall mode")
    elif "shadow_model" in plan or "shadow_codex" in plan:
        raise ValueError("old install plan cannot contain shadow options")
    if revision >= 9:
        if "reader_model" not in plan or "reader_codex" not in plan:
            raise ValueError("planned reader options are incomplete")
        reader_model = plan["reader_model"]
        reader_codex = plan["reader_codex"]
        if plan["recall_mode"] == "async":
            if type(reader_model) is not str or reader_model not in (READER_MODELS if revision >= 14 else LEGACY_READER_MODELS):
                raise ValueError("async mode requires an explicit supported reader model")
            planned_reader_config(reader_codex)
        elif reader_model is not None or reader_codex is not None:
            raise ValueError("reader options require async recall mode")
    elif "reader_model" in plan or "reader_codex" in plan:
        raise ValueError("old install plan cannot contain reader options")
    if revision >= 14:
        if "librarian_effort" not in plan:
            raise ValueError("planned librarian effort is missing; prepare a new plan")
        if plan["recall_mode"] == "async":
            resolve_policy(plan)
        elif plan["librarian_effort"] is not None:
            raise ValueError("librarian effort requires async recall mode")
    elif "librarian_effort" in plan:
        raise ValueError("old install plan cannot contain librarian effort; prepare a new plan")
    if revision >= 10:
        recording = plan.get("recording_mode")
        if type(recording) is not str or recording not in RECORDING_MODES:
            raise ValueError("invalid planned recording mode")
        if recording == "automatic" and plan["recall_mode"] != "async":
            raise ValueError("automatic recording requires async recall mode")
    elif "recording_mode" in plan:
        raise ValueError("old install plan cannot contain recording mode; prepare a new plan")
    if "global_preferences" in plan:
        if revision < 19 or plan["recall_mode"] != "async":
            raise ValueError("global preferences require a new async install plan")
        from target_policy import validate_global_preferences
        validate_global_preferences(plan["global_preferences"])
        expected = plan.get("global_preferences_service_sha256")
        if not isinstance(expected, str) or not HEX_SHA256.fullmatch(expected):
            raise ValueError("invalid global preference service pin")
    elif "global_preferences_service_sha256" in plan:
        raise ValueError("global preference service pin has no target")
    if "store_registry" in plan:
        if revision < 24 or plan["recall_mode"] != "async":
            raise ValueError("store registry requires a new async install plan")
        if "global_preferences" in plan:
            raise ValueError("store registry and global preferences cannot be combined")
        path, expected = plan["store_registry"], plan.get("store_registry_sha256")
        if (not isinstance(path, str) or "\x00" in path or len(path.encode()) > 4096
                or not Path(path).is_absolute() or str(Path(path)) != path
                or ".." in Path(path).parts or path.startswith("//")
                or not isinstance(expected, str) or not HEX_SHA256.fullmatch(expected)):
            raise ValueError("invalid store registry pin")
    elif "store_registry_sha256" in plan:
        raise ValueError("store registry pin has no target")
    if "memory_config" in plan:
        pin = plan["memory_config"]
        if (revision < 26 or plan["recall_mode"] != "async"
                or not isinstance(pin, dict) or set(pin) != {"path", "digest", "sources"}
                or not isinstance(pin["path"], str) or not Path(pin["path"]).is_absolute()
                or not isinstance(pin["digest"], str) or not HEX_SHA256.fullmatch(pin["digest"])
                or not isinstance(pin["sources"], list)
                or any(not isinstance(source, dict) or set(source) != {"role", "path", "sha256", "bytes"}
                    or not isinstance(source["role"], str) or len(source["role"]) > 128
                    or not isinstance(source["path"], str) or not Path(source["path"]).is_absolute()
                    or not isinstance(source["sha256"], str) or not HEX_SHA256.fullmatch(source["sha256"])
                    or type(source["bytes"]) is not int or not 0 <= source["bytes"] <= MAX_NATIVE_SOURCE_BYTES
                    for source in pin["sources"])
                or "store_registry" in plan or "global_preferences" in plan):
            raise ValueError("invalid native memory config pin")
        from store_registry import MAX_NATIVE_SNAPSHOT_BYTES
        if (sum(source["bytes"] for source in pin["sources"]) > MAX_NATIVE_SOURCE_TOTAL_BYTES
                or len(json.dumps(pin, ensure_ascii=False, separators=(",", ":")).encode()) > MAX_NATIVE_SNAPSHOT_BYTES):
            raise ValueError("native memory config pin exceeds byte bounds")
        from registry_runtime import native_memory_config_path
        native_memory_config_path(pin["path"])
    elif "existing_owner_binary" in plan:
        raise ValueError("retained owner binary requires native memory config")
    validate_native_managed(plan)
    root, prefix = Path(plan["project_root"]), Path(plan["prefix"])
    if not root.is_absolute() or not prefix.is_absolute() or not root.is_dir() or root.is_symlink():
        raise ValueError("plan paths must be absolute and project must be a real directory")
    if type(plan["port"]) is not int or not 1024 <= plan["port"] <= 65535:
        raise ValueError("invalid planned port")
    if plan["url"] != f"http://127.0.0.1:{plan['port']}/":
        raise ValueError("planned URL is not the loopback service endpoint")
    db = root / ".mneme/codex-memory.db"
    if plan["project_db"] != str(db) or prefix == db or db in prefix.parents:
        raise ValueError("planned project store path is inconsistent")
    if revision >= 40:
        validate_command(plan["python"], default="python3")
        validate_command(plan["mcp_command"], default="mneme-mcp")
        validate_command(plan["native_command"], default="mnemed")
    elif not Path(plan["python"]).is_absolute():
        raise ValueError("planned Python path must be absolute")
    from tag_config import FIELDS as tag_config_fields, validate as validate_tag_config
    if revision < 22 and set(plan) & tag_config_fields:
        raise ValueError("old install plan cannot contain tag stewardship; prepare a new plan")
    validate_tag_config(plan)
    if revision >= 24 and plan.get("host_platform") not in ("windows", "posix"):
        raise ValueError("invalid planned host platform")
    expected_destinations = (current_destinations(plan_platform(plan)) if revision >= 38 else
                             tuple((name + ".exe" if executable and plan_platform(plan) == "windows" else name, executable)
                                   for name, executable in V36_DESTINATIONS) if revision >= 36 else
                             tuple((name + ".exe" if executable and plan_platform(plan) == "windows" else name, executable)
                                   for name, executable in V34_DESTINATIONS) if revision >= 34 else
                             tuple((name + ".exe" if executable and plan_platform(plan) == "windows" else name, executable)
                                   for name, executable in V32_DESTINATIONS) if revision >= 32 else
                             tuple((name + ".exe" if executable and plan_platform(plan) == "windows" else name, executable)
                                   for name, executable in V30_DESTINATIONS) if revision >= 30 else
                             tuple((name + ".exe" if executable and plan_platform(plan) == "windows" else name, executable)
                                   for name, executable in V28_DESTINATIONS) if revision >= 28 else
                             tuple((name + ".exe" if executable and plan_platform(plan) == "windows" else name, executable)
                                   for name, executable in V26_DESTINATIONS) if revision >= 26 else
                             tuple((name + ".exe" if executable and plan_platform(plan) == "windows" else name, executable)
                                   for name, executable in V24_DESTINATIONS) if revision >= 24 else
                             V22_DESTINATIONS if revision >= 22 else
                             V20_DESTINATIONS if revision >= 20 else
                             V19_DESTINATIONS if revision >= 17 else
                             V16_DESTINATIONS if revision >= 16 else
                             V14_DESTINATIONS if revision >= 14 else
                             V13_DESTINATIONS if revision >= 11 else
                             V10_DESTINATIONS if revision == 10 else
                             V9_DESTINATIONS if revision == 9 else
                             V8_DESTINATIONS if revision == 8 else
                             V7_DESTINATIONS if revision >= 6 else
                             V5_DESTINATIONS if revision == 5 else
                             V4_DESTINATIONS if revision == 4 else
                             V3_DESTINATIONS if revision == 3 else LEGACY_DESTINATIONS)
    if not isinstance(plan["files"], list) or len(plan["files"]) != len(expected_destinations):
        raise ValueError("planned installation files are incomplete")
    for row, (destination, executable) in zip(plan["files"], expected_destinations):
        if not isinstance(row, dict) or row.get("destination") != destination or row.get("executable") is not executable:
            raise ValueError("planned installation files are inconsistent")
        if not isinstance(row.get("source"), str) or not Path(row["source"]).is_absolute():
            raise ValueError("planned source path must be absolute")
        if not isinstance(row.get("sha256"), str) or not HEX_SHA256.fullmatch(row["sha256"]):
            raise ValueError("planned source hash is invalid")
    expected = plan["expected_configs"]
    if not isinstance(expected, dict) or set(expected) != set(CONFIG_NAMES) or any(
        value is not None and (not isinstance(value, str) or not HEX_SHA256.fullmatch(value))
        for value in expected.values()
    ):
        raise ValueError("planned config fingerprints are invalid")
    if "activity" in plan:
        if revision < 36:
            raise ValueError("old install plan cannot contain activity; prepare a new plan")
        from registry_config import validate_activity_preparation
        validate_activity_preparation(plan["activity"], cwd=root,
            activity_mcp=plan["mcp_command"] if revision >= 40 else prefix / binary_destination(plan, "mneme-mcp"))
    if revision in EXISTING_SERVICE_REVISIONS:
        validate_existing_service(plan)
    return root, prefix


def hook_handlers(command, *, async_mode, recording, revision=CONFIG_REVISION, recall_mode="async", background_command=None):
    """One lifecycle shape for project installs and reviewed misc dispatchers."""
    events = {}
    names = (("SessionStart", "UserPromptSubmit", "Stop", "PostToolUse", "Interrupt", "SessionEnd")
             if async_mode else ("SessionStart", "UserPromptSubmit", "Stop"))
    for name in names:
        group = events.setdefault(name, [])
        handler = {"type": "command", "command": command,
                   "timeout": 3 if name == "Interrupt" or (revision >= 18 and name == "SessionEnd") else 5}
        if ((revision >= 4 and name in ("SessionStart", "UserPromptSubmit"))
                or (async_mode and name == "PostToolUse")):
            # Shadow's hook strictly caps complete output at 4096 bytes. Codex's
            # positive limit is an approximate token spill threshold; a preview
            # would make our delivered-card journal lie. Zero passes the bounded
            # output intact. Historical modes retain their 1600-token setting.
            exact_output = ((async_mode and name in ("UserPromptSubmit", "PostToolUse"))
                            or (revision >= 8 and recall_mode == "shadow"
                                and name == "UserPromptSubmit"))
            handler["additionalContextLimit"] = 0 if exact_output else 1600
        group.append({"hooks": [handler]})
        if async_mode and (name == "UserPromptSubmit" or
                           (recording and (name == "Stop" or (name == "SessionEnd" and revision < 18)))):
            group.append({"hooks": [{"type": "command", "command": background_command if background_command is not None else command + " --reader-background",
                                      "timeout": 5, "async": True}]})
    return events


def _planned_python_argv(plan, arguments):
    """Receipt revisions bind command bytes, not today's Python search policy."""
    windows = plan_platform(plan) == "windows"
    if 40 <= plan.get("config_revision", 1) < 44 and plan["python"] == "python3" and not windows:
        # Revisions 40-43 used first-PATH-match probing. Keep this exact spelling
        # for hash-checked uninstall/recovery of those installed configurations.
        script = ("for python in python3 python3.14 python3.13 python3.12 python3.11; do "
                  "if \"$python\" -c 'import sys; raise SystemExit(sys.version_info < (3, 11))' "
                  ">/dev/null 2>&1; then exec \"$python\" \"$@\"; fi; done; "
                  "printf '%s\\n' 'Mneme requires Python 3.11 or newer on PATH' >&2; exit 127")
        return ["/bin/sh", "-c", script, "mneme-python", *arguments]
    return python_hook_argv(plan["python"], arguments, windows=windows)


def planned_hook_handlers(plan):
    """Regenerate only the lifecycle definitions bound to this reviewed plan."""
    if "memory_config" in plan:
        # Native configuration is consumed by the one device hook actor; project
        # MCP setup must not install an additive second reader/recorder lifecycle.
        return {}
    prefix = Path(plan["prefix"])
    revision = plan.get("config_revision", 1)
    argv = [plan["python"], str(prefix / "lib/hooks.py"), "--config", str(prefix / "config/hooks.json")]
    if revision >= 40:
        argv = _planned_python_argv(plan, argv[1:])
    windows = plan_platform(plan) == "windows"
    command = hook_command(argv, windows=windows) if revision >= 24 else shlex.join(argv)
    background = (hook_command([*argv, "--reader-background"], windows=windows)
                  if revision >= 24 else command + " --reader-background")
    async_mode = revision >= 9 and plan["recall_mode"] == "async"
    recording = revision >= 10 and plan["recording_mode"] == "automatic"
    events = hook_handlers(command, async_mode=async_mode, recording=recording,
                           revision=revision, recall_mode=plan.get("recall_mode", "reminder"),
                           background_command=background)
    if revision >= 24 and async_mode and "store_registry" in plan:
        core_argv = [plan["python"], str(prefix / "lib/core_hook.py"),
                     "--hook-config", str(prefix / "config/hooks.json")]
        if revision >= 40:
            core_argv = _planned_python_argv(plan, core_argv[1:])
        # Startup core is a separate bounded read, not worker launch, learning
        # evidence or permission to record. Preserve its complete checked output.
        events["SessionStart"].append({"hooks": [{"type": "command",
            "command": hook_command(core_argv, windows=windows), "timeout": 15,
            "additionalContextLimit": 0}]})
    return events


def config_outputs(plan, before):
    root, prefix = Path(plan["project_root"]), Path(plan["prefix"])
    revision = plan.get("config_revision", 1)
    toml_value = lambda value: json.dumps(value, ensure_ascii=revision < 24)
    existing = before["config.toml"]
    old = b"" if existing is None else existing
    parsed = tomllib.loads(old.decode())
    managed = plan.get("native_managed")
    server = managed["mcp"]["name"] if managed is not None else SERVER
    if server in parsed.get("mcp_servers", {}) or BEGIN.encode() in old:
        raise ValueError(f"{server} configuration already exists; do not overwrite it")
    key = toml_value(server) if managed is not None else server
    block = f"\n{BEGIN}\n[mcp_servers.{key}]\n"
    if managed is not None:
        launcher = _planned_python_argv(plan, [str(prefix / "lib/launcher.py"),
            "--service-config", str(prefix / "config/service.json"),
            "--memory-config", plan["memory_config"]["path"], "--cwd", str(root),
            "--expected-db-id", managed["db_id"], "--native-cli", plan["native_command"],
            "--native-mcp", plan["mcp_command"]])
        block += (f"command = {toml_value(launcher[0])}\n"
                  f"args = {toml_value(launcher[1:])}\n")
        if not managed["mcp"]["inherited"]:
            block += "startup_timeout_sec = 30\n"
        # Do not serialize inherited environment/secrets or narrow the native
        # router's toolset. Codex merges this same-name launch override with the
        # user's filters, approvals and other per-server settings.
    elif "memory_config" in plan:
        mcp_selector = plan["mcp_command"] if revision >= 40 else str(prefix / binary_destination(plan, "mneme-mcp"))
        block += (f"command = {toml_value(mcp_selector)}\n"
                  f"args = {toml_value(['--memory-config', plan['memory_config']['path'], '--cwd', str(root)])}\n"
                  "startup_timeout_sec = 30\n")
    elif revision >= 3:
        launcher = [plan["python"], str(prefix / "lib/launcher.py"), "--service-config", str(prefix / "config/service.json")]
        if revision >= 40:
            launcher = _planned_python_argv(plan, launcher[1:])
        block += (f"command = {toml_value(launcher[0])}\n"
                  f"args = {toml_value(launcher[1:])}\n"
                  "startup_timeout_sec = 30\n")
    else:
        block += f"url = {json.dumps(plan['url'])}\n"
    # Reviewed older receipts must retain their exact tool authorization and
    # output hashes for recovery/uninstall. Episode enrollment is revision 7;
    # canonical SAVE authorization starts only with revision 12, concern with 13.
    tools = NATIVE_TOOLS if "memory_config" in plan else TOOLS if revision >= 13 else SAVE_TOOLS if revision >= 12 else EPISODE_TOOLS if revision >= 7 else LEGACY_TOOLS
    if managed is None:
        block += f"enabled_tools = {json.dumps(tools)}\n"
    if revision >= 2 and managed is None:
        # No server default, sandbox change, or global approval. Only the
        # tools this project integration actually uses are preapproved.
        block += "".join(f"[mcp_servers.{SERVER}.tools.{name}]\n"
                         "approval_mode = \"approve\"\n" for name in tools)
    if "activity" in plan:
        from registry_config import ACTIVITY_SERVER
        if ACTIVITY_SERVER in parsed.get("mcp_servers", {}):
            raise ValueError("mneme_session_activity configuration already exists; do not overwrite it")
        activity = plan["activity"]["mcp"]
        block += (f"[mcp_servers.{ACTIVITY_SERVER}]\n"
                  f"command = {toml_value(activity['command'])}\n"
                  f"args = {toml_value(activity['args'])}\n"
                  "startup_timeout_sec = 10\n"
                  'enabled_tools = ["session_activity"]\n'
                  f"[mcp_servers.{ACTIVITY_SERVER}.tools.session_activity]\n"
                  'approval_mode = "approve"\n')
    block = (block + f"{END}\n").encode()
    toml = old + block
    tomllib.loads(toml.decode())
    raw_hooks = before["hooks.json"]
    hooks = {} if raw_hooks is None else json.loads(raw_hooks)
    if not isinstance(hooks, dict) or not isinstance(hooks.get("hooks", {}), dict):
        raise ValueError("existing hooks.json must contain an object of event arrays")
    events = hooks.setdefault("hooks", {})
    generated = planned_hook_handlers(plan)
    for name, handlers in generated.items():
        group = events.setdefault(name, [])
        if not isinstance(group, list):
            raise ValueError(f"existing {name} hooks are not an array")
        group.extend(handlers)
    return {"config.toml": toml, "hooks.json": json_bytes(hooks)}


def prepare(project_root, prefix, mnemed, mcp, port, *, recall_mode="reminder", python="python3",
            program_root=None, library_helper=None, enroll_existing=False,
            shadow_model=None, shadow_codex=None, reader_model=None, reader_codex=None,
            recording_mode="off", librarian_effort=None, global_preferences=None,
            store_registry=None, existing_service_config=None, tag_stewardship=None, tag_guide_id=None, memory_config=None,
            activity_root=None, activity_config=None, activity_device=None,
            mcp_command="mneme-mcp", native_command="mnemed", native_managed=None):
    python = prepare_python_selector(python, windows=HOST_PLATFORM == "windows")
    mcp_command = validate_command(mcp_command, default="mneme-mcp")
    native_command = validate_command(native_command, default="mnemed")
    project_root, prefix = Path(project_root).absolute(), Path(prefix).absolute()
    native_pin = None
    activity = None
    if activity_root is not None:
        from registry_config import prepare_activity
        activity = prepare_activity(cwd=project_root, activity_root=activity_root,
            activity_config=activity_config, activity_device=activity_device,
            activity_mcp=mcp_command)
    elif activity_config is not None or activity_device is not None:
        raise ValueError("activity options require explicit --activity-root")
    if memory_config is not None:
        if (store_registry is not None or global_preferences is not None or reader_model is not None
                or librarian_effort is not None or recording_mode != "off"
                or tag_stewardship is not None or tag_guide_id is not None
                or recall_mode not in ("reminder", "async") or library_helper is not None or enroll_existing):
            raise ValueError("native memory config cannot be combined with independent hook policy or enrollment")
    elif native_managed is not None:
        raise ValueError("native managed startup requires native memory config")
    if not project_root.is_dir() or project_root.is_symlink():
        raise ValueError("project root must be an existing real directory")
    if prefix.exists():
        raise ValueError("installation prefix must be absent; choose a new staged version")
    if type(port) is not int or not 1024 <= port <= 65535:
        raise ValueError("port must be in 1024..65535")
    if type(recall_mode) is not str or recall_mode not in RECALL_MODES:
        raise ValueError("recall mode must be automatic, reminder, shadow, or async")
    if type(recording_mode) is not str or recording_mode not in RECORDING_MODES:
        raise ValueError("recording mode must be off or automatic")
    if recording_mode == "automatic" and recall_mode != "async":
        raise ValueError("automatic recording requires --recall-mode async")
    if memory_config is not None:
        from registry_runtime import native_memory_config_path
        memory_path = native_memory_config_path(memory_config)
        from store_registry import native_select
        snapshot = native_select(str(memory_path), str(Path(mnemed).resolve(strict=True)), str(project_root)).snapshot
        native_pin = memory_config_input(memory_path, Path(mnemed).resolve(strict=True), project_root, snapshot)
        background = snapshot["config"]["background"]
        recall_mode, reader_model = "async", background.get("model") or MODEL
        librarian_effort = background.get("effort") or "medium"
    from tag_config import prepared_fields
    tag_fields = prepared_fields(recording_mode, tag_stewardship, tag_guide_id)
    if recall_mode != "async" and tag_guide_id is not None:
        raise ValueError("tag guide requires --recall-mode async")
    registry_pin = None
    if store_registry is not None:
        if global_preferences is not None:
            raise ValueError("store registry and global preferences cannot be combined")
        if recall_mode != "async":
            raise ValueError("store registry requires --recall-mode async")
        store_registry, registry_pin = store_registry_input(store_registry)
    preference_pin = None
    if global_preferences is not None:
        if recall_mode != "async":
            raise ValueError("global preferences require --recall-mode async")
        from target_policy import global_preferences_policy, validate_global_preferences
        global_preferences = validate_global_preferences(global_preferences)
        policy = global_preferences_policy({"schema": "mneme.codex-hooks.config.v10",
            "project_root": str(project_root), "memory_mode": "async",
            "global_preferences": global_preferences})
        preference_pin = digest(Path(policy.service_config).read_bytes())
    shadow_pin = None
    if recall_mode == "shadow":
        if type(shadow_model) is not str or shadow_model not in SHADOW_MODELS:
            raise ValueError("shadow mode requires --shadow-model (gpt-5.6-sol or gpt-5.6-terra)")
        shadow_pin = prepare_reader_codex("codex" if shadow_codex is None else shadow_codex)["reader_codex"]
    elif shadow_model is not None or shadow_codex is not None:
        raise ValueError("--shadow-model and --shadow-codex require --recall-mode shadow")
    reader_pin = None
    if recall_mode == "async":
        if type(reader_model) is not str or reader_model not in READER_MODELS:
            raise ValueError("async mode requires --reader-model gpt-6.1-sol")
        librarian_effort = "medium" if librarian_effort is None else librarian_effort
        resolve_policy({"reader_model":reader_model,"librarian_effort":librarian_effort})
        reader = prepare_reader_codex("codex" if reader_codex is None else reader_codex)
        reader_pin = reader["reader_codex"]
    elif reader_model is not None or reader_codex is not None or librarian_effort is not None:
        raise ValueError("--reader-model, --reader-codex and --librarian-effort require --recall-mode async")
    if enroll_existing and library_helper is None:
        raise ValueError("--enroll-existing requires --library-helper")
    db_exists = (project_root / ".mneme/codex-memory.db").exists()
    helper_pin = None
    if library_helper is not None:
        helper_source = Path(library_helper).resolve(strict=True)
        if not helper_source.is_file():
            raise ValueError("library helper must be a file")
        helper_pin = {"source": str(helper_source),
                      "sha256": digest(helper_source.read_bytes())}
    enrollment = ("none" if helper_pin is None or (db_exists and not enroll_existing)
                  else "existing" if db_exists else "fresh")
    if enrollment == "none":
        helper_pin = None
    dot_codex = project_root / ".codex"
    if dot_codex.is_symlink() or (dot_codex.exists() and not dot_codex.is_dir()):
        raise ValueError("project .codex must be a real directory or absent")
    programs = Path(program_root or Path(__file__).parent).resolve()
    files = []
    platform = HOST_PLATFORM
    extension = ".exe" if platform == "windows" else ""
    for source, destination, executable in [
        (Path(mnemed), "bin/mnemed" + extension, True),
        (Path(mcp), "bin/mneme-mcp" + extension, True),
        *[(programs / name, "lib/" + name, False) for name in PROGRAMS],
    ]:
        source = source.resolve(strict=True)
        if not source.is_file() or (executable and not is_executable(source)):
            raise ValueError(f"not a usable installation source: {source}")
        files.append({"source": str(source), "destination": destination,
                      "sha256": digest(source.read_bytes()), "executable": executable})
    if recall_mode != "async":
        tag_fields = {}
    before = {name: read_optional(dot_codex / name) for name in CONFIG_NAMES}
    plan = {"schema": SCHEMA, "config_revision": CONFIG_REVISION, "host_platform": platform,
            "recall_mode": recall_mode,
            "recording_mode": recording_mode,
            **tag_fields,
            "shadow_model": shadow_model, "shadow_codex": shadow_pin,
            "reader_model": reader_model, "reader_codex": reader_pin,
            "librarian_effort": librarian_effort,
            "library_helper": helper_pin, "library_enrollment": enrollment,
            "project_root": str(project_root), "prefix": str(prefix),
            "python": python, "mcp_command": mcp_command, "native_command": native_command,
            "port": port, "url": f"http://127.0.0.1:{port}/",
            "project_db": str(project_root / ".mneme/codex-memory.db"),
            "files": files,
            "expected_configs": {name: None if data is None else digest(data)
                                 for name, data in before.items()},
            "authority": "project-only operator; direct feedback disabled",
            "does_not": ["initialize stores", "start service", "trust hooks", "edit global config"]}
    if activity is not None:
        plan["activity"] = activity
        plan["does_not"] = ["edit global Codex config" if item == "edit global config" else item
                            for item in plan["does_not"]]
        plan["does_not"].append("remove shared activity bindings or diagnostic data on uninstall/failure")
    if global_preferences is not None:
        plan["global_preferences"] = global_preferences
        plan["global_preferences_service_sha256"] = preference_pin
        plan["authority"] = "project operator with explicit personal collaboration preference lane; direct feedback disabled"
    if store_registry is not None:
        plan["store_registry"] = store_registry
        plan["store_registry_sha256"] = registry_pin
        plan["authority"] = "project operator with explicit named store registry; direct feedback disabled"
    if native_pin is not None:
        plan["memory_config"] = native_pin
        plan["authority"] = "native config grants; one device hook actor; no project hook lifecycle"
        plan["does_not"].append("install project hooks; device hooks are prepared separately")
    if native_managed is not None:
        plan["native_managed"] = native_managed
        validate_native_managed(plan)
    if existing_service_config is not None:
        plan["config_revision"] = EXISTING_SERVICE_REVISION
        plan["existing_service_config"] = existing_service_input(existing_service_config)
        revalidate_existing_service(plan)
    if activity is not None:
        plan["authority"] += "; explicit shared activity binding and private diagnostic root"
    plan["plan_sha256"] = plan_hash(plan)
    config_outputs(plan, before)  # Validate the exact merge before emitting a plan.
    return plan


def _originals(plan, prefix, dot_codex):
    """Read verified backups, falling back to untouched live originals."""
    originals = {}
    for name in CONFIG_NAMES:
        expected = plan["expected_configs"][name]
        if expected is None:
            originals[name] = None
            continue
        try:
            data = (prefix / "backups" / name).read_bytes()
        except FileNotFoundError:
            data = read_optional(dot_codex / name)
        if data is None or digest(data) != expected:
            raise ValueError(f"original {name} is unavailable or backup changed")
        originals[name] = data
    return originals


def _output_hashes(outputs):
    return {name: digest(data) for name, data in outputs.items()}


def _supported_output_hashes(plan, originals):
    # The plan revision selects exactly its prepared transport and approvals.
    # Older receipts retain their six-file inventory and original output.
    return (_output_hashes(config_outputs(plan, originals)),)


def _restore_known_outputs(dot_codex, originals, after_hashes):
    """Restore only matching installer output; accept already restored files."""
    for name in CONFIG_NAMES:
        current = fingerprint(dot_codex / name)
        original_hash = None if originals[name] is None else digest(originals[name])
        if current not in (original_hash, after_hashes[name]):
            raise ValueError(f"{name} changed since install; remove owned entries manually")
    for name in CONFIG_NAMES:
        current = fingerprint(dot_codex / name)
        original_hash = None if originals[name] is None else digest(originals[name])
        if current == original_hash:
            continue
        if current != after_hashes[name]:
            raise ValueError(f"{name} changed during restoration; inspect manually")
        data = originals[name]
        if data is None:
            (dot_codex / name).unlink()
        else:
            private_write(dot_codex / name, data)


def planned_reader_config(reader):
    """Translate a reviewed symbolic command or retained explicit pin without I/O."""
    if isinstance(reader, str):
        config = {"reader_codex": reader}
    elif (isinstance(reader, dict) and set(reader) == {"source", "sha256"}
          and isinstance(reader["source"], str)):
        config = {"reader_codex": reader["source"], "reader_codex_sha256": reader["sha256"]}
    else:
        raise ValueError("mode requires codex from PATH or an absolute Codex executable with an optional explicit pin")
    validate_reader_codex(config)
    return config


def runtime_configs(plan):
    """Pure installer-owned runtime outputs; also binds exact hook-review scope."""
    root, prefix = Path(plan["project_root"]), Path(plan["prefix"])
    shadow_codex, reader_codex = plan["shadow_codex"], plan["reader_codex"]
    dynamic = plan.get("config_revision", 1) >= 40
    service = {"binary": plan["mcp_command"] if dynamic else str(prefix / binary_destination(plan, "mneme-mcp")), "project_db": plan["project_db"],
               "working_directory": str(root), "port": plan["port"], "state_dir": str(prefix / "run")}
    if "existing_service_config" in plan:
        service = dict(validate_existing_service(plan)["config"])
    if "native_managed" in plan:
        service["database_id"] = plan["native_managed"]["db_id"]
    hook = {"schema": "mneme.codex-hooks.config.v2", "project_root": str(root),
            "state_dir": str(root / ".mneme/codex-hook-state"),
            "service_config": str(prefix / "config/service.json"),
            "recall_mode": plan["recall_mode"]}
    if plan["recall_mode"] == "shadow":
        hook.pop("recall_mode")
        hook.update(schema="mneme.codex-hooks.config.v3", memory_mode="shadow",
                    shadow_model=plan["shadow_model"],
                    **{key.replace("reader_", "shadow_", 1): value
                       for key, value in planned_reader_config(shadow_codex).items()})
    elif plan["recall_mode"] == "async":
        hook.pop("recall_mode")
        hook.update(schema="mneme.codex-hooks.config.v8", memory_mode="async",
                    librarian_effort=plan["librarian_effort"], recording_mode=plan["recording_mode"],
                    reader_model=plan["reader_model"], **planned_reader_config(reader_codex))
        for field in ("tag_stewardship", "tag_guide_id"):
            if field in plan:
                hook[field] = plan[field]
        if "global_preferences" in plan:
            hook.update(schema="mneme.codex-hooks.config.v10",
                        global_preferences=dict(plan["global_preferences"]))
        if "store_registry" in plan:
            hook.update(schema="mneme.codex-hooks.config.v12", store_registry=plan["store_registry"])

    if "memory_config" in plan:
        hook = {"schema": "mneme.codex-hooks.config.v13", "memory_config": plan["memory_config"]["path"],
                "native_cli": plan["native_command"] if dynamic else str(prefix / binary_destination(plan, "mnemed")),
                "state_dir": str(root / ".mneme/codex-hook-state"),
                **planned_reader_config(reader_codex)}
    return service, hook


def apply(plan):
    root, prefix = validate_plan(plan)
    if plan.get("config_revision", 1) not in SUPPORTED_APPLY_REVISIONS:
        raise ValueError("old install plan cannot be applied by this installer; prepare a new plan")
    if plan_platform(plan) != HOST_PLATFORM:
        raise ValueError("install plan targets a different host platform; prepare a new plan")
    revalidate_existing_service(plan)
    revalidate_native_managed(plan)
    revalidate_store_registry(plan)
    if "activity" in plan:
        from registry_config import revalidate_activity
        revalidate_activity(plan["activity"])
    if "global_preferences" in plan:
        from target_policy import global_preferences_policy
        policy = global_preferences_policy({"schema": "mneme.codex-hooks.config.v10",
            "project_root": plan["project_root"], "memory_mode": "async",
            "global_preferences": plan["global_preferences"]})
        if digest(Path(policy.service_config).read_bytes()) != plan["global_preferences_service_sha256"]:
            raise ValueError("global preference service changed since prepare; prepare a new plan")
    dot_codex = root / ".codex"
    if dot_codex.is_symlink():
        raise ValueError("project .codex must not be a symlink")
    before = {name: read_optional(dot_codex / name) for name in CONFIG_NAMES}
    for name, data in before.items():
        if (None if data is None else digest(data)) != plan["expected_configs"][name]:
            raise ValueError(f"configuration changed since prepare: {name}; prepare a new plan")
    for row in plan["files"]:
        if digest(Path(row["source"]).read_bytes()) != row["sha256"]:
            raise ValueError("installation source changed; prepare a new plan")
    revalidate_memory_config(plan)
    helper = plan["library_helper"]
    if helper is not None and digest(Path(helper["source"]).read_bytes()) != helper["sha256"]:
        raise ValueError("library helper changed; prepare a new plan")
    shadow_codex = plan["shadow_codex"]
    if isinstance(shadow_codex, dict) and digest(Path(shadow_codex["source"]).read_bytes()) != shadow_codex["sha256"]:
        raise ValueError("shadow Codex executable changed; prepare a new plan")
    reader_codex = plan["reader_codex"]
    if isinstance(reader_codex, dict) and digest(Path(reader_codex["source"]).read_bytes()) != reader_codex["sha256"]:
        raise ValueError("reader Codex executable changed; prepare a new plan")
    db_exists = (root / ".mneme/codex-memory.db").exists()
    if plan["library_enrollment"] == "fresh" and db_exists:
        raise ValueError("project store appeared since prepare; prepare a new plan")
    if plan["library_enrollment"] == "existing" and not db_exists:
        raise ValueError("project store disappeared since prepare; prepare a new plan")
    after = config_outputs(plan, before)
    ensure_private_directory(prefix, parents=True, exist_ok=False)
    for name in ("bin", "lib", "config", "run", "backups"):
        ensure_private_directory(prefix / name, parents=False, exist_ok=False)
    # Leave a reviewable recovery record before touching any Codex configuration.
    private_write(prefix / "pending.json", json_bytes(plan))
    for name, data in before.items():
        if data is not None:
            private_write(prefix / "backups" / name, data)
    for row in plan["files"]:
        destination = prefix / row["destination"]
        atomic_write(destination, Path(row["source"]).read_bytes(), mode=0o700 if row["executable"] else 0o600)
        if digest(destination.read_bytes()) != row["sha256"]:
            raise ValueError("installed artifact does not match prepared source")
    if helper is not None:
        library_destination = prefix / "lib/library.py"
        private_write(library_destination, Path(helper["source"]).read_bytes())
        if digest(library_destination.read_bytes()) != helper["sha256"]:
            raise ValueError("installed library helper does not match prepared source")
        private_write(prefix / "config/enrollment.json", json_bytes({
            "mode": plan["library_enrollment"], "project_root": str(root),
            "helper_sha256": helper["sha256"]}))
    service, hook = runtime_configs(plan)

    private_write(prefix / "config/service.json", json_bytes(service))
    private_write(prefix / "config/hooks.json", json_bytes(hook))
    if "activity" in plan:
        from registry_config import publish_activity
        # Shared enrollment is intentionally retained if later project config
        # publication fails. The pending plan identifies it; recovery/uninstall
        # must not erase another workspace's diagnostic binding or observations.
        publish_activity(plan["activity"])
    config_directory(dot_codex)
    after_hashes = _output_hashes(after)
    receipt = {"schema": SCHEMA, "plan": plan,
               "installed_configs": after_hashes,
               "hook_trust": "requires user review; not granted by installer",
               "service": "not started", "store": "not initialized"}
    receipt_data = json_bytes(receipt)
    try:
        for name, data in after.items():
            if fingerprint(dot_codex / name) != plan["expected_configs"][name]:
                raise ValueError(f"configuration changed during install: {name}")
            private_write(dot_codex / name, data)
        private_write(prefix / "receipt.json", receipt_data)
    except Exception:
        # A write may have replaced its target before failing during fsync.
        # Compare both files, not just calls which returned successfully.
        _restore_known_outputs(dot_codex, before, after_hashes)
        receipt_path = prefix / "receipt.json"
        if fingerprint(receipt_path) == digest(receipt_data):
            receipt_path.unlink()
        raise
    pending_recovery = None
    try:
        (prefix / "pending.json").unlink()
    except OSError:
        # Receipt plus installed hashes are authoritative; `recover` can clear
        # the leftover pending marker without touching the configuration.
        pending_recovery = str(prefix / "pending.json")
    result = {"status": "configured", "receipt": str(prefix / "receipt.json"),
              "hook_trust": receipt["hook_trust"], "service": receipt["service"], "store": receipt["store"]}
    if "activity" in plan:
        result["activity_config"] = plan["activity"]["config_path"]
        result["activity_root"] = plan["activity"]["manifest"]["root"]
    if pending_recovery is not None:
        result["pending_recovery"] = pending_recovery
    return result


def uninstall(receipt_path):
    receipt = json.loads(Path(receipt_path).read_text(encoding="utf-8"))
    if receipt.get("schema") != SCHEMA:
        raise ValueError("unsupported receipt")
    plan = receipt["plan"]
    root, prefix = validate_plan(plan)
    if Path(receipt_path).absolute() != prefix / "receipt.json":
        raise ValueError("receipt path does not match installation prefix")
    dot_codex = root / ".codex"
    originals = _originals(plan, prefix, dot_codex)
    after_hashes = receipt.get("installed_configs")
    if after_hashes not in _supported_output_hashes(plan, originals):
        raise ValueError("receipt installed hashes do not match the prepared configuration")
    _restore_known_outputs(dot_codex, originals, after_hashes)
    return {"status": "configuration removed", "preserved": ["all memory stores", "runtime", "hook state", "shared activity bindings and diagnostic data"],
            "service": "not stopped; stop explicitly using service.py"}


def recover(pending_path):
    """Finalize committed install or abort a pending one without erasing data."""
    pending_path = Path(pending_path).absolute()
    plan = json.loads(pending_path.read_text(encoding="utf-8"))
    root, prefix = validate_plan(plan)
    if pending_path != prefix / "pending.json":
        raise ValueError("pending path does not match installation prefix")
    dot_codex = root / ".codex"
    originals = _originals(plan, prefix, dot_codex)
    supported_hashes = _supported_output_hashes(plan, originals)
    receipt_path = prefix / "receipt.json"
    if receipt_path.exists():
        receipt = json.loads(receipt_path.read_text(encoding="utf-8"))
        if receipt.get("schema") != SCHEMA or receipt.get("plan") != plan or receipt.get("installed_configs") not in supported_hashes:
            raise ValueError("receipt does not match pending installation")
        after_hashes = receipt["installed_configs"]
        if all(fingerprint(dot_codex / name) == after_hashes[name] for name in CONFIG_NAMES):
            pending_path.unlink()
            return {"status": "configured", "receipt": str(receipt_path)}
        # Receipt replacement may have succeeded but its fsync failed, after
        # which apply attempted to roll the configuration back. Resume that
        # rollback if every live file is still a known before/after image.
        _restore_known_outputs(dot_codex, originals, after_hashes)
        receipt_path.unlink()
        pending_path.unlink()
        return {"status": "aborted", "preserved": ["all memory stores", "runtime", "hook state", "shared activity bindings and diagnostic data"],
                "prefix": str(prefix)}
    live_hashes = {name: fingerprint(dot_codex / name) for name in CONFIG_NAMES}
    candidates = [hashes for hashes in supported_hashes
                  if all(live_hashes[name] in (
                      None if originals[name] is None else digest(originals[name]), hashes[name]
                  ) for name in CONFIG_NAMES)]
    if not candidates:
        raise ValueError("pending configuration is neither original nor a known installed image")
    _restore_known_outputs(dot_codex, originals, candidates[0])
    pending_path.unlink()
    return {"status": "aborted", "preserved": ["all memory stores", "runtime", "hook state", "shared activity bindings and diagnostic data"],
            "prefix": str(prefix)}


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    commands = parser.add_subparsers(dest="command", required=True)
    prep = commands.add_parser("prepare")
    for name in ("project-root", "prefix", "mnemed", "mcp", "output"):
        prep.add_argument("--" + name, type=Path, required=True)
    prep.add_argument("--port", type=int, default=18765)
    prep.add_argument("--existing-service-config", type=Path,
                      help="pin an explicit matching managed-local owner; retain its binary and state paths")
    prep.add_argument("--recall-mode", choices=RECALL_MODES, default="reminder",
                      help="reminder (default); automatic, shadow, and async are explicit experimental opt-ins")
    prep.add_argument("--recording-mode", choices=RECORDING_MODES, default="off",
                      help="off (default); automatic records selected completed-task outcomes and requires async recall")
    prep.add_argument("--tag-stewardship", action=argparse.BooleanOptionalAction, default=None,
                      help="fresh automatic recording enables owner-scoped tag stewardship by default")
    prep.add_argument("--tag-guide-id", help="optional canonical guide node ID in the selected owner")
    prep.add_argument("--shadow-model", choices=SHADOW_MODELS,
                      help="required with --recall-mode shadow; no implicit model")
    prep.add_argument("--shadow-codex",
                      help="shadow command: codex from PATH (default), or an absolute executable")
    prep.add_argument("--reader-model", choices=READER_MODELS,
                      help="required with --recall-mode async; explicitly selected librarian model")
    prep.add_argument("--librarian-effort", choices=EFFORTS,
                      help="async low/medium/high resource preset; default medium (not a spending cap)")
    prep.add_argument("--mcp-command", default="mneme-mcp", help="runtime MCP selector, separate from --mcp artifact source")
    prep.add_argument("--native-command", default="mnemed", help="runtime native CLI selector, separate from --mnemed artifact source")
    prep.add_argument("--python", default="python3",
                      help="runtime Python selector: python3 from PATH (default), or a lexical absolute executable")
    prep.add_argument("--reader-codex",
                      help="async reader command: codex from PATH (default), or an absolute executable (no automatic hash pin)")
    prep.add_argument("--memory-config", type=Path,
                      help="native memory.toml policy; installs configured MCP only; device hooks prepared separately")
    prep.add_argument("--store-registry", type=Path,
                      help="explicit reviewed named-store registry; async only; not combined with global preferences")
    prep.add_argument("--global-preferences", type=Path,
                      help="explicit reviewed JSON owner binding for cross-project preferences; async only")
    prep.add_argument("--library-helper", type=Path,
                      help="optional reviewed integrations/library/library.py source to pin")
    prep.add_argument("--enroll-existing", action="store_true",
                      help="explicitly authorize descriptor enrollment for an existing store")
    prep.add_argument("--activity-root", type=Path,
                      help="explicitly enroll shared activity manifest/private root and separate project activity MCP entry")
    prep.add_argument("--activity-config", type=Path,
                      help="activity manifest override; defaults to platform Mneme config directory/activity.json")
    prep.add_argument("--activity-device", help="optional activity device display label")
    ap = commands.add_parser("apply")
    ap.add_argument("--plan", type=Path, required=True)
    remove = commands.add_parser("uninstall")
    remove.add_argument("--receipt", type=Path, required=True)
    rescue = commands.add_parser("recover")
    rescue.add_argument("--pending", type=Path, required=True)
    args = parser.parse_args()
    try:
        if args.command == "prepare":
            plan = prepare(args.project_root, args.prefix, args.mnemed, args.mcp, args.port,
                           recall_mode=args.recall_mode, python=args.python, library_helper=args.library_helper,
                           enroll_existing=args.enroll_existing,
                           shadow_model=args.shadow_model, shadow_codex=args.shadow_codex,
                           reader_model=args.reader_model, reader_codex=args.reader_codex,
                           librarian_effort=args.librarian_effort,
                           recording_mode=args.recording_mode,
                           tag_stewardship=args.tag_stewardship, tag_guide_id=args.tag_guide_id,
                           existing_service_config=args.existing_service_config,
                           store_registry=args.store_registry, memory_config=args.memory_config,
                           activity_root=args.activity_root, activity_config=args.activity_config,
                           activity_device=args.activity_device,
                           mcp_command=args.mcp_command, native_command=args.native_command,
                           global_preferences=(global_preferences_input(args.global_preferences)
                                               if args.global_preferences is not None else None))
            args.output.parent.mkdir(parents=True, exist_ok=True)
            private_write(args.output, json_bytes(plan))
            result = {"status": "prepared", "plan": str(args.output), "project_root": plan["project_root"],
                      "prefix": plan["prefix"], "project_db": plan["project_db"], "url": plan["url"],
                      "recall_mode": plan["recall_mode"],
                      "recording_mode": plan["recording_mode"],
                      "library_enrollment": plan["library_enrollment"],
                      "does_not": plan["does_not"]}
        elif args.command == "apply":
            result = apply(json.loads(args.plan.read_text(encoding="utf-8")))
        elif args.command == "uninstall":
            result = uninstall(args.receipt)
        else:
            result = recover(args.pending)
        print(json.dumps(result))
    except (ValueError, OSError, KeyError, TypeError) as error:
        print(json.dumps({"error": str(error), "status": "not completed"}), file=sys.stderr)
        return 1
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
