#!/usr/bin/env python3
"""Finite, device-local project setup. No publishing/downloads; exact shipped hook trust through native Codex RPCs."""
from __future__ import annotations

import argparse
import importlib.util
import json
import os
from pathlib import Path
import re
import shutil
import socket
import subprocess
import sys
import uuid
import tomllib

import install
from hook_trust import trust_project_hooks
from profile import load_profile
from misc_binding import _codex_configured
from service import ServiceConfig, ensure_ready, load_config, _expected_catalog
from mcp_client import McpClient
from host_files import open_directory, open_regular
from host_platform import default_data_dir, executable_name, is_executable

SCHEMA = "mneme.project-init.v1"
DB_ID = re.compile(r"[0-7][0-9A-HJKMNP-TV-Z]{25}\Z")


def read_json(path, limit=1024 * 1024):
    raw = install.read_optional(Path(path))
    if raw is None:
        return None
    if len(raw) > limit:
        raise ValueError(f"configuration exceeds limit: {path}")
    def pairs(items):
        result = {}
        for key, value in items:
            if key in result:
                raise ValueError(f"duplicate configuration field in {path}")
            result[key] = value
        return result
    return json.loads(raw, object_pairs_hook=pairs)


def executable(value, name, sibling=None):
    source = value
    if source is None or str(source) == name:
        source = sibling if sibling and sibling.is_file() else shutil.which(executable_name(name))
    if source is None:
        raise ValueError(f"{name} executable unavailable; install it explicitly or pass --{name if name != 'mneme-mcp' else 'mcp'}-binary PATH (no builds/downloads performed)")
    source = Path(source).resolve(strict=True)
    if not is_executable(source):
        raise ValueError(f"{name} executable is not usable")
    return source


def native(mnemed, root, *args):
    # Native init/status are bounded and offline only when no configured owner exists.
    result = subprocess.run([str(mnemed), "--json", *map(str, args)], cwd=root,
                            stdin=subprocess.DEVNULL, capture_output=True, timeout=40)
    if len(result.stdout) > 512 * 1024 or len(result.stderr) > 64 * 1024:
        raise ValueError("native setup response exceeds its bound")
    if result.returncode:
        raise ValueError(result.stderr.decode(errors="replace")[:2048].strip() or "native setup refused")
    return json.loads(result.stdout)


def registry_path(selection, explicit):
    chosen = selection["library_config"]
    if explicit and chosen and Path(explicit).resolve() != chosen.resolve():
        raise ValueError("--library-config conflicts with the project's explicit library selection")
    if explicit or chosen:
        return Path(explicit or chosen).absolute()
    if os.name != "nt" and not (os.environ.get("XDG_DATA_HOME") or os.environ.get("HOME")):
        raise ValueError("cannot locate device-local registry; set HOME/XDG_DATA_HOME or --library-config")
    return default_data_dir() / "libraries/personal/library.json"


def selected_store_registry(explicit, existing):
    """Discover global config only for fresh setup; never reroute an existing install."""
    from store_registry import default_registry_path
    if existing is not None:
        configured = existing[2].get("store_registry")
        if explicit is not None:
            chosen, _ = install.store_registry_input(explicit)
            if configured is None or chosen != str(configured):
                raise ValueError("--store-registry conflicts with existing integration; prepare an explicit replacement")
        return configured
    if explicit is not None:
        return install.store_registry_input(explicit)[0]
    path = default_registry_path()
    # A dangling symlink is configured-but-invalid, not an absent default.
    if path.exists() or path.is_symlink():
        return install.store_registry_input(path)[0]
    return None


def configured_integration(root):
    """Adopt only the existing explicit shipped launcher, never guess an owner."""
    raw = install.read_optional(root / ".codex/config.toml")
    config = {} if raw is None else tomllib.loads(raw.decode())
    server = config.get("mcp_servers", {}).get(install.SERVER)
    if server is None:
        return None
    if not isinstance(server, dict) or server.get("enabled", True) is not True:
        raise ValueError("existing project Mneme integration is explicitly disabled; init will not enable it")
    args = server.get("args")
    if isinstance(args, list) and len(args) > 3:
        # New receipts use the exact shipped dynamic-Python envelope. Do not
        # unwrap arbitrary shell commands when identifying an existing owner.
        candidate = args[-3:]
        if [server.get("command"), *args] == install.python_hook_argv("python3", candidate):
            args = candidate
    if (not isinstance(args, list) or len(args) != 3 or not all(isinstance(x, str) for x in args)
            or Path(args[0]).name != "launcher.py" or args[1] != "--service-config"
            or not Path(args[2]).is_absolute()):
        raise ValueError("existing Mneme integration is not the shipped local launcher; preserve it and configure its owner explicitly")
    service_path = Path(args[2])
    hook_path = service_path.parent / "hooks.json"
    # Validate the existing hook schema through its existing reader, not another schema.
    from hooks import _config
    hook = _config(hook_path)
    if hook["project_root"] != root or Path(hook.get("service_config", "")) != service_path:
        raise ValueError("existing integration belongs to another project/service")
    return service_path, hook_path, hook


def catalog(config):
    with McpClient(config.url, token_env=config.token_env or None, timeout=3) as client:
        value = client.call_tool("databases", {})
    if not _expected_catalog(config, value):
        raise ValueError("owner catalog does not match the exact project path/alias")
    db_id = value[0].get("db_id")
    if not isinstance(db_id, str) or not DB_ID.fullmatch(db_id):
        raise ValueError("owner catalog has no canonical database identity")
    return db_id


def import_library(path):
    spec = importlib.util.spec_from_file_location("mneme_setup_library", path)
    module = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(module)
    return module


def native_selection(mnemed, root, memory_config, *, expected=None):
    """Acquire permitted discovery metadata once; subsequent guards replay offline."""
    from store_registry import native_select
    options = {} if expected is None else {"expected_selection_digest": expected}
    return native_select(str(memory_config), str(mnemed), str(root), **options).snapshot


def native_project_route(snapshot, db_id, root, *, url=None):
    rows = [row for row in snapshot["active"] if row["store"]["id"] == db_id]
    if len(rows) != 1:
        raise ValueError("native project owner is not uniquely active; review configuration (no fallback attempted)")
    row = rows[0]
    owner = row["store"]["owner"]
    if (not row["eligible"]["read"] or owner["db"] != "project"
            or owner.get("path") != str(root / ".mneme/codex-memory.db")
            or (url is not None and owner["url"] != url)
            or owner.get("token_env") is not None):
        raise ValueError("native project read grant/owner identity differs from local setup; no alternate owner started")
    return row


def native_setup_baseline(snapshot, root, name):
    """Check known exclusions before creating a new, not-yet-identified store."""
    project = snapshot["project"]
    if snapshot["profile"] != "default" or project.get("legacy_library") is not None:
        raise ValueError("native project policy is excluded or selects a legacy library; preserve it and migrate explicitly")
    for allow in (snapshot["config"].get("workspace", {}).get("allow_stores"),
                  project.get("legacy_allow_stores")):
        if allow is not None and name not in allow:
            raise ValueError("native workspace does not admit this project store; no fallback attempted")


def native_local_claims(snapshot):
    """Read only native-pinned local declarations, not imported policy assertions."""
    names, ids = set(), set()
    for source in snapshot["sources"]:
        if source["role"] not in ("user", "project"):
            continue
        raw = install.read_optional(Path(source["path"]))
        if raw is None or install.digest(raw) != source["sha256"]:
            raise ValueError("native source changed during project setup; reselect before retrying")
        # Native selection already validated this TOML. This is a declaration
        # inventory for safe append, not a second policy/selection implementation.
        for name, store in tomllib.loads(raw.decode()).get("stores", {}).items():
            names.add(name)
            ids.add(store["id"])
    return names, ids


def restore_native_overlay(path, before, after):
    current = install.read_optional(path)
    if current == before:
        return
    if current != after:
        raise ValueError("native overlay changed concurrently; retained it for manual recovery")
    if before is None:
        path.unlink()
    else:
        install.private_write(path, before)


def native_setup_result(result, snapshot, row, service_path, receipt, store):
    result.update(status="configured", stage="complete", store=store,
        integration="native", db_id=row["store"]["id"], owner_url=row["store"]["owner"]["url"],
        service_config=str(service_path), receipt=str(receipt), selection_digest=snapshot["digest"],
        memory_config=(snapshot.get("request") or {}).get("user_path"),
        local_registration=False, library_config=None,
        recording="eligible (native device policy)" if row["eligible"]["record"] else "off (native policy)",
        hippocampus="native device policy unchanged",
        hook_trust={"status":"device_actor", "trusted":0,
                    "reason":"native device actor configuration/trust unchanged; no project hooks installed or trusted"},
        recording_activation="device actor presence/trust/execution not verified by init; configuration unchanged",
        published=False, sharing="unchanged; no enrollment/publication performed")


def setup_native(args, result, root, selection):
    """Adopt one project owner without replacing the configured native system."""
    if (args.library_config is not None or getattr(args, "store_registry", None) is not None
            or args.no_recording or args.codex_binary is not None):
        raise ValueError("native init uses configured policy; --library-config/--store-registry/--no-recording/--codex-binary cannot override it")
    if selection["library_config"] is not None or selection.get("stores") is not None:
        raise ValueError("existing legacy project selector requires explicit migration; native init will not overwrite it")
    dot, db = root / ".mneme", root / ".mneme/codex-memory.db"
    overlay, prefix = dot / "memory.toml", dot / "codex-integration"
    if (dot / "cli.json").exists() or (dot / "cli.json").is_symlink():
        raise ValueError("existing legacy CLI owner requires explicit migration; native init will not compete with it")
    for name in ("memory.db", "current", "generations"):
        if (dot / name).exists() or (dot / name).is_symlink():
            raise ValueError("ambiguous project storage; native init will not create a parallel database")
    if dot.exists():
        for index, path in enumerate(dot.iterdir()):
            if index >= 4096:
                raise ValueError("project storage directory exceeds setup inspection bound")
            if path.name != db.name and path.suffix.lower() in (".db", ".sqlite", ".sqlite3"):
                raise ValueError("ambiguous custom project storage; configure its owner explicitly")
    if db.is_symlink() or (db.exists() and (not db.is_file() or db.stat().st_nlink != 1)):
        raise ValueError("project store must be a single-link real file")
    mnemed = executable(args.mnemed, "mnemed")
    memory = Path(args.memory_config)
    os.environ["MNEME_CLIENT_BINARY"] = str(mnemed)
    result["stage"] = "native_policy"
    baseline = native_selection(mnemed, root, memory)
    native_mcp = install.native_mcp_input(memory, root)

    # A completed native install is resumed in place. Never open its live DB
    # offline beside the managed owner and never regenerate its project policy.
    receipt_path = prefix / "receipt.json"
    if prefix.exists():
        pending = prefix / "pending.json"
        if pending.is_file():
            recovered = install.recover(pending)
            if recovered["status"] == "aborted":
                prefix.rename(dot / ("codex-integration-aborted-" + uuid.uuid4().hex))
        if prefix.exists():
            receipt = read_json(receipt_path)
            if receipt is None or "native_managed" not in receipt.get("plan", {}):
                raise ValueError("existing integration is not native managed setup; preserve it and migrate explicitly")
            plan = receipt["plan"]
            install.validate_plan(plan)
            if plan["project_root"] != str(root) or plan["memory_config"]["path"] != str(memory):
                raise ValueError("existing native integration selects another project/configuration")
            previous_mcp = plan["native_managed"]["mcp"]
            if any(native_mcp[key] != previous_mcp[key] for key in ("name", "command", "config_path", "inherited")):
                raise ValueError("inherited native MCP registration changed; review/reprepare the project override instead of reusing a different route")
            hashes = receipt["installed_configs"]
            if any(install.fingerprint(root / ".codex" / name) != hashes[name] for name in install.CONFIG_NAMES):
                raise ValueError("native project integration changed since install; review it instead of overwriting")
            service_path = prefix / "config/service.json"
            expected_service, _ = install.runtime_configs(plan)
            if install.read_optional(service_path) != install.json_bytes(expected_service):
                raise ValueError("generated native service configuration changed; review it before rerunning init")
            config = load_config(service_path)
            db_id = plan["native_managed"]["db_id"]
            row = native_project_route(baseline, db_id, root, url=config.url)
            if (not isinstance(config, ServiceConfig) or config.database_id != db_id
                    or config.database_name != "project" or config.database_path != db
                    or config.working_directory != root or config.token_env):
                raise ValueError("native service no longer matches the guarded project owner")
            if args.port is not None and args.port != config.port:
                raise ValueError("--port conflicts with existing native owner")
            native_selection(mnemed, root, memory, expected=baseline["digest"])
            result["stage"] = "owner_start"
            ensure_ready(config, timeout=10, lock_timeout=2)
            native_setup_result(result, baseline, row, service_path, receipt_path, "adopted")
            return

    # Pin all known local definitions and native boundary bytes before any
    # creation. A local declaration may override imported discovery, never a
    # user/project declaration with the same canonical identity or name.
    local_names, local_ids = native_local_claims(baseline)
    name = root.name if re.fullmatch(r"[A-Za-z0-9_.-]{1,128}", root.name) else "project"
    if not db.exists():
        native_setup_baseline(baseline, root, name)
        if name in local_names:
            raise ValueError("native project would duplicate a local store name; no store created")
    project_raw = install.read_optional(root / ".codex/config.toml")
    project_config = {} if project_raw is None else tomllib.loads(project_raw.decode())
    if (native_mcp["name"] in project_config.get("mcp_servers", {})
            or (project_raw is not None and install.BEGIN.encode() in project_raw)):
        raise ValueError("project MCP integration already exists; preserve it instead of installing a second one")
    mcp_command = str(args.mcp_binary) if args.mcp_binary is not None else native_mcp["command"]
    from host_platform import resolve_command
    resolve_command(mcp_command, default="mneme-mcp")
    mcp = executable(args.mcp_binary, "mneme-mcp", mnemed.parent / executable_name("mneme-mcp"))
    if args.port is None:
        with socket.socket() as listener:
            listener.bind(("127.0.0.1", 0))
            port = listener.getsockname()[1]
    else:
        port = args.port
    if type(port) is not int or not 1024 <= port <= 65535:
        raise ValueError("port must be in 1024..65535")
    url = f"http://127.0.0.1:{port}/"
    before = install.read_optional(overlay)
    native_selection(mnemed, root, memory, expected=baseline["digest"])
    result["stage"] = "store_admission"
    store = "adopted" if db.exists() else "created"
    if not db.exists():
        install.config_directory(dot)
        native(mnemed, root, "--db", db, "capture", "init")
    verified = native(mnemed, root, "--db", db, "capture", "inspect")
    db_id = verified.get("db_id")
    if not isinstance(db_id, str) or not DB_ID.fullmatch(db_id):
        raise ValueError("native store inspection supplied no canonical identity")
    result.update(store=store, db_id=db_id)
    existing_names = [row["name"] for row in baseline["stores"] if row["store"]["id"] == db_id]
    if len(existing_names) > 1:
        raise ValueError("multiple native names claim the project identity; review them instead of guessing")
    if existing_names:
        name = existing_names[0]
        existing_row = next(row for row in baseline["stores"] if row["store"]["id"] == db_id)
        if not existing_row["admitted"] or not existing_row["eligible"]["read"]:
            raise ValueError("existing native project is not admitted for reads; no fallback attempted")
    native_setup_baseline(baseline, root, name)
    if name in local_names or db_id in local_ids:
        raise ValueError("native project would duplicate a local store name/identity; explicit bindings are not overwritten")
    q = lambda value: json.dumps(value, ensure_ascii=False)
    after = (b'schema = "mneme.config.v1"\n' if before is None else before) + (
        f'\n[stores.{q(name)}]\nid = {q(db_id)}\n'
        f'owner = {{ url = {q(url)}, db = "project", path = {q(str(db))} }}\n'
        'active = { workspace = "project" }\n').encode()
    if len(after) > install.MAX_NATIVE_SOURCE_BYTES:
        raise ValueError("project native overlay exceeds native source bound")
    result["stage"] = "native_overlay"
    installed = None
    plan = None
    owned_prefix = False
    try:
        if install.read_optional(overlay) != before:
            raise ValueError("native overlay changed before publication")
        install.private_write(overlay, after)
        selected = native_selection(mnemed, root, memory)
        native_project_route(selected, db_id, root, url=url)
        plan = install.prepare(root, prefix, mnemed, mcp, port,
            memory_config=memory, python=getattr(args, "python", "python3"),
            mcp_command=mcp_command, native_command="mnemed",
            native_managed={"db_id": db_id, "mcp": native_mcp})
        result["stage"] = "integration_install"
        installed = install.apply(plan)
        owned_prefix = True
        # Codex configuration is itself a native boundary source. Acquire the
        # final receipt after ALL owned configuration writes, never pin the
        # pre-install digest as the launcher's lasting selection.
        result["stage"] = "native_readback"
        final = native_selection(mnemed, root, memory)
        row = native_project_route(final, db_id, root, url=url)
        native_selection(mnemed, root, memory, expected=final["digest"])
        service_path = prefix / "config/service.json"
        expected_service, _ = install.runtime_configs(plan)
        if install.read_optional(service_path) != install.json_bytes(expected_service):
            raise ValueError("generated native service configuration changed during setup")
        config = load_config(service_path)
        if (not isinstance(config, ServiceConfig) or config.database_id != db_id
                or config.database_name != "project" or config.database_path != db
                or config.working_directory != root or config.token_env):
            raise ValueError("generated service does not match the guarded native owner")
    except Exception:
        if installed is not None:
            install.uninstall(installed["receipt"])
        elif (prefix / "pending.json").is_file() and read_json(prefix / "pending.json") == plan:
            install.recover(prefix / "pending.json")
            owned_prefix = True
        restore_native_overlay(overlay, before, after)
        if owned_prefix and prefix.exists():
            prefix.rename(dot / ("codex-integration-aborted-" + uuid.uuid4().hex))
        raise
    result["stage"] = "owner_start"
    # The validated route/installer are now committed and resumable. A readiness
    # failure retains that exact pair and the DB; rerunning init retries this
    # same route, not another owner or a fresh database.
    ensure_ready(config, timeout=10, lock_timeout=2)
    native_setup_result(result, final, row, service_path, installed["receipt"], store)


def setup(args, result):
    requested = Path(args.root).absolute()
    if requested.is_symlink() or not requested.is_dir():
        raise ValueError("project root must be an existing real directory")
    os.close(open_directory(requested))
    root = requested.resolve(strict=True)
    result["project_root"] = str(root)
    dot = root / ".mneme"
    for path in (dot, root / ".codex"):
        if path.is_symlink() or (path.exists() and not path.is_dir()):
            raise ValueError(f"setup requires a real directory or absent path: {path}")
        if path.exists():
            os.close(open_directory(path))
    # Preserve exclusions on both lexical and canonical ancestry. Setup is not
    # an escape hatch from an inherited private/isolated project.
    seen = set()
    for chain in (requested, root):
        for index, parent in enumerate((chain, *chain.parents)):
            if index >= 64:
                raise ValueError("project setup ancestry exceeds its bound")
            if parent in seen:
                continue
            seen.add(parent)
            inherited = load_profile(parent)
            if inherited["mode"] != "default":
                raise ValueError(f"project profile is explicitly {inherited['mode']}; init preserves that choice")
            if parent != root and ((parent / ".mneme").exists() or (parent / ".mneme").is_symlink()
                    or _codex_configured(parent / ".codex")):
                raise ValueError("project inherits an existing Mneme boundary; initialize its actual root instead")
    selection = load_profile(root)
    if selection["mode"] != "default":
        raise ValueError(f"project profile is explicitly {selection['mode']}; init preserves that choice")
    if getattr(args, "memory_config", None) is not None:
        return setup_native(args, result, root, selection)
    registry = registry_path(selection, args.library_config)
    # Do not create a parallel conventional database beside an activated/other store.
    for name in ("memory.db", "current", "generations"):
        path = dot / name
        if path.exists() or path.is_symlink():
            raise ValueError(f"ambiguous project storage at {path}; adopt/configure that existing store explicitly, not a second database")
    if dot.exists():
        entries = []
        for index, path in enumerate(dot.iterdir()):
            if index >= 4096:
                raise ValueError("project storage directory exceeds setup inspection bound")
            entries.append(path)
        if any(path.name != "codex-memory.db" and path.suffix.lower() in (".db", ".sqlite", ".sqlite3")
               for path in entries):
            raise ValueError("ambiguous custom project storage; configure/adopt the existing store explicitly")
    db = dot / "codex-memory.db"
    if db.is_symlink() or (db.exists() and (not db.is_file() or db.stat().st_nlink != 1)):
        raise ValueError("project store must be a single-link real file")
    if db.exists():
        os.close(open_regular(db))
    mnemed = executable(args.mnemed, "mnemed")
    # This overrides only this short-lived setup's native transport adapter.
    os.environ["MNEME_CLIENT_BINARY"] = str(mnemed)
    toml_raw = install.read_optional(root / ".codex/config.toml")
    project_toml = {} if toml_raw is None else tomllib.loads(toml_raw.decode())
    toml_hooks = project_toml.get("hooks", {})
    if not isinstance(toml_hooks, dict) or toml_hooks.get("enabled", True) is not True:
        raise ValueError("project TOML hooks are explicitly disabled or malformed; init will not enable them")
    hooks_doc = read_json(root / ".codex/hooks.json")
    if hooks_doc is not None:
        if not isinstance(hooks_doc, dict) or hooks_doc.get("enabled", True) is not True:
            raise ValueError("project hooks are explicitly disabled or malformed; init will not enable them")
        if hooks_doc.get("hooks", {}).get("enabled", True) is not True:
            raise ValueError("project hooks are explicitly disabled; init will not enable them")
    existing = configured_integration(root)
    prefix = dot / "codex-integration"
    if existing is None and prefix.exists():
        pending = prefix / "pending.json"
        if pending.is_file():
            result["stage"] = "installer_recovery"
            recovered = install.recover(pending)
            if recovered["status"] == "aborted":
                prefix.rename(dot / ("codex-integration-aborted-" + uuid.uuid4().hex))
            existing = configured_integration(root)
        if existing is None and prefix.exists():
            raise ValueError(f"unclaimed integration prefix {prefix}; preserve it and review pending.json/receipt.json before retrying")
    store_registry = selected_store_registry(getattr(args, "store_registry", None), existing)
    expected_db_id = None
    previous_owner = read_json(dot / "cli.json", 8192)
    if existing is None:
        if _codex_configured(root / ".codex"):
            raise ValueError("other existing Mneme/managed project integration; init preserves it instead of installing a second one")
        if previous_owner is not None:
            raise ValueError("project CLI owner already exists without a matching local Codex integration; init will not replace or compete with it")
        mcp_command = str(args.mcp_binary) if args.mcp_binary is not None else "mneme-mcp"
        # Validate the runtime selector before any storage creation. A packaged
        # sibling may supply artifact bytes, but is not an implicit launch pin.
        from host_platform import resolve_command
        resolve_command(mcp_command, default="mneme-mcp")
        mcp = executable(args.mcp_binary, "mneme-mcp", mnemed.parent / executable_name("mneme-mcp"))
        # Persist the command choice, not whichever executable happens to be
        # installed today. Hook trust resolves its one-shot executable below.
        reader_codex = args.codex_binary if args.codex_binary is not None else "codex"
        if args.port is None:
            with socket.socket() as listener:
                listener.bind(("127.0.0.1", 0))
                port = listener.getsockname()[1]
        else:
            port = args.port
        # All configuration/precondition validation precedes storage creation.
        plan = install.prepare(root, prefix, mnemed, mcp, port, recall_mode="async",
                               recording_mode="off" if args.no_recording else "automatic",
                               reader_model=install.READER_MODELS[0], reader_codex=reader_codex,
                               python=getattr(args, "python", "python3"),
                               mcp_command=mcp_command, native_command="mnemed",
                               store_registry=store_registry)
        result["stage"] = "store_admission"
        install.config_directory(dot)
        if db.exists():
            verified = native(mnemed, root, "--db", db, "capture", "inspect")
            expected_db_id = verified.get("db_id")
            if not isinstance(expected_db_id, str) or not DB_ID.fullmatch(expected_db_id):
                raise ValueError("native store inspection supplied no canonical identity")
            result["store"] = "adopted"
        else:
            native(mnemed, root, "--db", db, "capture", "init")
            verified = native(mnemed, root, "--db", db, "capture", "inspect")
            expected_db_id = verified.get("db_id")
            if not isinstance(expected_db_id, str) or not DB_ID.fullmatch(expected_db_id):
                raise ValueError("created store inspection supplied no canonical identity")
            result["store"] = "created"
        result["stage"] = "integration_install"
        applied = install.apply(plan)
        result["receipt"] = applied["receipt"]
        existing = configured_integration(root)
    else:
        result["store"] = "adopted"
    service_path, hook_path, hook = existing
    config = load_config(service_path)
    if (not isinstance(config, ServiceConfig) or config.database_name != "project"
            or config.database_path != db or config.working_directory != root):
        raise ValueError("existing service is not the exact local project owner; no alternate owner started")
    if args.port is not None and args.port != config.port:
        raise ValueError("--port conflicts with existing owner; init does not reroute configured services")
    if previous_owner is not None:
        if (not isinstance(previous_owner, dict)
                or set(previous_owner) - {"schema", "url", "database", "db_id", "token_env"}
                or not isinstance(previous_owner.get("db_id"), str)
                or not DB_ID.fullmatch(previous_owner["db_id"])
                or previous_owner.get("schema") != "mneme.cli.owner.v1"
                or previous_owner.get("url") != config.url
                or previous_owner.get("database") != config.database_name):
            raise ValueError("existing CLI owner route conflicts; no alternate owner started")
    enrollment = read_json(service_path.parent / "enrollment.json", 8192)
    if enrollment is not None and enrollment.get("mode") not in (None, "none"):
        raise ValueError("existing integration has publishing enrollment; review/disable it explicitly before device-local init")
    result.update(service_config=str(service_path), owner_url=config.url)
    if store_registry is not None:
        result["store_registry"] = str(store_registry)
    result["stage"] = "owner_start"
    ensure_ready(config, timeout=10)
    db_id = catalog(config)
    if expected_db_id is not None and expected_db_id != db_id:
        raise ValueError("project database identity changed between adoption and owner startup; no CLI binding written")
    result["db_id"] = db_id
    owner = {"schema": "mneme.cli.owner.v1", "url": config.url,
             "database": config.database_name, "db_id": db_id}
    if config.token_env:
        owner["token_env"] = config.token_env
    if previous_owner is not None and previous_owner != owner:
        raise ValueError("existing CLI owner identity/route conflicts; init will not replace it")
    # Explicit recording-off changes only the existing recorder choice. Ordinary
    # reruns never upgrade an existing off/reminder/shadow configuration.
    if args.no_recording and hook.get("recording_mode") == "automatic":
        raw_hook = read_json(hook_path)
        raw_hook["recording_mode"] = "off"
        install.private_write(hook_path, install.json_bytes(raw_hook))
        hook["recording_mode"] = "off"
    result["recording"] = hook.get("recording_mode", "off")
    result["hippocampus"] = hook.get("memory_mode", hook.get("recall_mode", "reminder"))
    result["stage"] = "local_registration"
    registered = import_library(args.library_helper).register_local_project(
        registry, root, db_id, database=config.database_name,
        owner_endpoint={key: owner[key] for key in ("url", "token_env") if key in owner})
    if registered and selection["library_config"] is None and store_registry is None:
        install.private_write(dot / "profile.json", install.json_bytes({"schema": "mneme.profile.v1",
            "mode": "default", "library_config": str(registry)}))
    if previous_owner is None:
        install.private_write(dot / "cli.json", install.json_bytes(owner))
    result.update(status="configured", stage="complete", local_registration=registered,
                  library_config=str(registry), hook_trust="unchanged; init does not grant Codex hook trust",
                  recording_activation="requires Codex hook trust; review project hooks in Codex",
                  published=False, sharing="unchanged; no enrollment/publication performed")
    if args.no_trust_hooks:
        result["hook_trust"] = {"status":"not_requested", "trusted":0,
            "reason":"explicit --no-trust-hooks; review installed project hooks in Codex"}
    else:
        try:
            codex = executable(args.codex_binary or hook.get("reader_codex") or hook.get("shadow_codex"), "codex")
            result["hook_trust"] = trust_project_hooks(codex, root, service_path)
        except Exception:
            result["hook_trust"] = {"status":"pending", "trusted":0, "reason":"codex_unavailable"}
    result["recording_activation"] = ("enabled hook definitions trusted; fresh-session execution not yet verified"
        if result["hook_trust"]["status"] == "trusted" else "hook trust pending; review project hooks in Codex")



def main(argv=None):
    parser = argparse.ArgumentParser(description=__doc__)
    for name in ("root", "mnemed", "library-helper"):
        parser.add_argument("--" + name, required=True, type=Path)
    for name in ("mcp-binary", "library-config", "store-registry", "memory-config"):
        parser.add_argument("--" + name, type=Path)
    parser.add_argument("--python", default="python3",
                        help="runtime Python selector: python3 from PATH (default), or a lexical absolute executable")
    parser.add_argument("--codex-binary",
                        help="optional absolute reader executable (no automatic hash pin); default codex follows PATH and is resolved separately for hook trust")
    parser.add_argument("--port", type=int)
    parser.add_argument("--no-recording", action="store_true")
    parser.add_argument("--no-trust-hooks", action="store_true")
    parser.add_argument("--json", action="store_true")
    args = parser.parse_args(argv)
    result = {"schema": SCHEMA, "status": "incomplete", "stage": "preflight", "published": False}
    try:
        setup(args, result)
    except Exception as error:
        result["error"] = str(error)[:2048]
        result["recovery"] = "Preserved data/configuration; resolve the reported conflict, then rerun mnemed init. An installer pending.json is recovered on rerun; do not delete memory data."
    if args.json:
        print(json.dumps(result, ensure_ascii=True))
    elif result["status"] == "configured" and result.get("integration") == "native":
        print(f"Project memory {result['store']}: {result['project_root']}\nNative owner {result['db_id']} ready; native policy unchanged (no publishing performed).\nNo project lifecycle hooks installed. Recording: {result['recording']}.\nDevice actor presence/trust/execution not verified. Managed MCP startup is configured; a fresh Codex session is still required.")
    elif result["status"] == "configured":
        print(f"Project memory {result['store']}: {result['project_root']}\nOwner {result['db_id']} ready; {('locally registered' if result['local_registration'] else 'not registered: existing exclusion/withdrawal preserved')} (no publishing performed).\nHippocampus: {result['hippocampus']}; recording: {result['recording']}.\nHook trust: {result['hook_trust']['status']} ({result['hook_trust']['trusted']} trusted enabled hooks). Fresh-session execution is not verified.")
        if result["hook_trust"]["status"] != "trusted":
            print(f"Hook review remains manual ({result['hook_trust'].get('reason', 'pending')}); use Codex /hooks. Project store/owner remain configured.")
    else:
        print(f"Project init incomplete at {result['stage']}: {result['error']}\n{result['recovery']}", file=sys.stderr)
    return 0 if result["status"] == "configured" else 1


if __name__ == "__main__":
    raise SystemExit(main())
