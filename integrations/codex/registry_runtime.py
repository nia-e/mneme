"""Bind one v12 hook session to a checked named-store selection.

Static authoring, workspace admission and durable session fencing are separate:
none of these functions contacts an owner, opens a database or invents a primary
store for unconfigured work. Existing ledgers are retained, never reset on upgrade.
"""
from pathlib import Path
import hashlib
import re

from librarian_policy import resolve as resolve_budget
from misc_binding import canonical_path, validate_binding
from store_registry import (Selection, project_store, resolve, admit_project_workspace)
from tag_config import FIELDS as TAG_FIELDS, validate as validate_tags

SCHEMA = "mneme.codex-hooks.config.v12"
BASE_FIELDS = {"schema", "state_dir", "store_registry", "memory_mode", "reader_model",
               "librarian_effort", "recording_mode", "reader_codex", "reader_codex_sha256"}


def validate_static(value):
    if not isinstance(value, dict) or value.get("schema") != SCHEMA:
        raise ValueError("named stores require hook schema v12")
    scope = value.get("memory_scope", "project")
    if scope not in ("project", "registry"):
        raise ValueError("v12 hook scope must be project or registry")
    required = BASE_FIELDS | ({"memory_scope"} if scope == "registry" else {"project_root", "service_config"})
    if not required <= set(value) or set(value) - required - TAG_FIELDS - {"memory_scope", "reader_auth"}:
        raise ValueError("invalid v12 hook configuration fields")
    if value.get("memory_mode") != "async" or value.get("recording_mode") not in ("off", "automatic"):
        raise ValueError("v12 requires explicit async and recording policies")
    resolve_budget(value)
    validate_tags(value)
    for field in ("state_dir", "store_registry", "reader_codex", "reader_auth", "project_root", "service_config"):
        if field in value:
            if not isinstance(value[field], str):
                raise ValueError("hook configuration paths must be strings")
            canonical_path(value[field])
    if not isinstance(value.get("reader_codex_sha256"), str) or not re.fullmatch(r"[0-9a-f]{64}", value["reader_codex_sha256"]):
        raise ValueError("invalid reader executable hash")
    return dict(value)


def _project(config, cwd):
    # Profile/nested admission precedes even loading the selected owner config.
    root = config["project_root"]
    admit_project_workspace(cwd, root)
    from service import load_config, ConnectConfig, RemoteConfig
    service = load_config(config["service_config"])
    if (service.database_name != "project" or not isinstance(service, (ConnectConfig, RemoteConfig))
            and service.database_path != root / ".mneme" / "codex-memory.db"):
        raise ValueError("configured project owner differs from project boundary")
    from recording_jobs import load_project_focus
    focus = load_project_focus(config)
    config["_project_focus"] = focus
    recording = config["recording_mode"] == "automatic"
    return project_store(config["service_config"], root, database_path=str(service.database_path),
        purpose=focus or "Source-bound knowledge and experience belonging to this project.",
        read={"enabled": True, "core": True},
        record={"enabled": recording, "kinds": ["lesson", "episode", "possibility"],
                "evidence_kinds": ["user_statement", "assistant_assertion", "tool_call", "tool_result", "memory_delivery"],
                "allow_associations": True, "max_summary_bytes": 512, "max_body_bytes": 2048},
        maintenance={"enabled": recording and config.get("tag_stewardship", False),
                     "tag_guide_id": config.get("tag_guide_id")})


def load(value, path, raw, *, workspace_binding=None, cwd=None):
    """Freeze one config/selection witness for the existing single worker."""
    from hooks import MAX_CONFIG, _bounded_json
    config = validate_static(value)
    scope = config.get("memory_scope", "project")
    config["memory_scope"] = scope
    if scope == "registry":
        binding = validate_binding(workspace_binding)
        config["workspace_binding"] = binding
        config["project_root"] = Path(binding["workspace_root"])
        current = Path(cwd or binding["workspace_origin"])
        current.resolve(strict=True).relative_to(config["project_root"])
    else:
        if workspace_binding is not None:
            raise ValueError("project registry hooks cannot accept unconfigured workspace binding")
        config["project_root"] = Path(config["project_root"]).resolve(strict=True)
        if not config["project_root"].is_dir():
            raise ValueError("project root must exist")
        current = Path(cwd or config["project_root"])
    for field in ("state_dir", "store_registry", "reader_codex", "reader_auth", "service_config"):
        if field in config:
            config[field] = Path(config[field]).resolve()
    config["selection_cwd"] = current
    config["recall_mode"] = "automatic"
    config["_config_path"] = Path(path).resolve()
    config["_registry_config_sha256"] = hashlib.sha256(raw).hexdigest()
    project = _project(config, current) if scope == "project" else None
    selected = resolve(current, registry_path=config["store_registry"], project_binding=project,
                       mode="async" if config["recording_mode"] == "automatic" else "read")
    config["frozen_selection"] = selected.to_dict()
    if _bounded_json(Path(path), MAX_CONFIG, with_raw=True)[1] != raw:
        raise ValueError("hook config changed while selecting stores; reload")
    from recording_jobs import _config_digest
    config["_reader_config_pin"] = _config_digest(config)
    selection(config)
    return config


def selection(config):
    """Revalidate the exact snapshot, never resolve a replacement destination."""
    if config.get("_registry_config_sha256") is not None:
        from hooks import _bounded_json, MAX_CONFIG
        raw = _bounded_json(Path(config["_config_path"]), MAX_CONFIG, with_raw=True)[1]
        if hashlib.sha256(raw).hexdigest() != config["_registry_config_sha256"]:
            raise ValueError("hook policy changed; reload before work")
    from store_registry import selection_from_dict
    current = selection_from_dict(config["frozen_selection"])
    if str(config.get("selection_cwd", current.cwd)) != current.cwd:
        raise ValueError("selected workspace changed")
    if config.get("workspace_binding") is not None:
        validate_binding(config["workspace_binding"])
    if config.get("schema") != NATIVE_SCHEMA and config.get("_registry_origin_scope", config.get("memory_scope")) == "project":
        from recording_jobs import load_project_focus
        origin = {**config, "memory_scope": "project"}
        if load_project_focus(origin) != config.get("_project_focus"):
            raise ValueError("project purpose changed; reload before work")
    return current


def config_for_store(config, store):
    """An explicit owner overlay, not another session or another allowance."""
    current = selection(config)
    if current.named(store.name) != store:
        raise ValueError("store does not match frozen selection")
    return {**config, "memory_scope": "registry",
            "_registry_origin_scope": config.get("_registry_origin_scope", config.get("memory_scope")),
            "service_config": Path(store.service_config),
            "selected_store": store.name, "_store_purpose": store.purpose,
            "tag_stewardship": store.maintenance.enabled and (config.get("schema") == NATIVE_SCHEMA or config.get("recording_mode") == "automatic"),
            "tag_guide_id": store.maintenance.tag_guide_id}


def admit_session(config, event):
    """No old/new actor overlap or reset of paid accounting on config changes."""
    from hooks import _with_state, HookError
    selected = selection(config)
    current = Path(event["cwd"])
    if current != Path(selected.cwd):
        # A new cwd may be a meaningful policy boundary. Reprepare the session
        # rather than implicitly changing its frozen destination/paid work.
        raise HookError("named-store session workspace changed; start a fresh session")
    witness = {"selection": selected.digest, "hook_config": str(config["_config_path"])}
    def bind(data, _now):
        previous = data.get("registry_binding")
        if previous is None:
            if data.get("turns") or data.get("context_epoch", 0):
                raise HookError("registry upgrade requires quiesced workers and a fresh session; retain old ledgers")
            data["registry_binding"] = witness
            return None, True
        if previous != witness:
            raise HookError("registry selection changed; quiesce worker and start a fresh session")
        return None, False
    _with_state(config["state_dir"], event["session_id"], bind, blocking=False)


NATIVE_SCHEMA = "mneme.codex-hooks.config.v13"
NATIVE_FIELDS = {"schema", "state_dir", "memory_config", "native_cli", "reader_codex", "reader_codex_sha256"}


def validate_native_static(value):
    if (not isinstance(value, dict) or value.get("schema") != NATIVE_SCHEMA
            or not NATIVE_FIELDS <= set(value) or set(value) - NATIVE_FIELDS - {"reader_auth", "workspace_adapters"}):
        raise ValueError("invalid native hook adapter fields")
    for key in ("state_dir", "memory_config", "native_cli", "reader_codex", "reader_auth"):
        if key in value: canonical_path(value[key])
    if not isinstance(value["reader_codex_sha256"], str) or not re.fullmatch(r"[0-9a-f]{64}", value["reader_codex_sha256"]):
        raise ValueError("invalid reader executable hash")
    adapters = value.get("workspace_adapters", [])
    if not isinstance(adapters, list) or len(adapters) > 64:
        raise ValueError("invalid workspace adapter inventory")
    roots = set()
    for adapter in adapters:
        if not isinstance(adapter, dict) or set(adapter) != {"root", "state_dir", "reader_codex", "reader_codex_sha256"}:
            raise ValueError("invalid workspace adapter")
        for key in ("root", "state_dir", "reader_codex"): canonical_path(adapter[key])
        root_path = Path(adapter["root"]).resolve(strict=True)
        if not root_path.is_dir():
            raise ValueError("workspace adapter root must exist")
        root = str(root_path)
        if root in roots or not isinstance(adapter["reader_codex_sha256"], str) or not re.fullmatch(r"[0-9a-f]{64}", adapter["reader_codex_sha256"]):
            raise ValueError("duplicate workspace adapter or invalid reader hash")
        roots.add(root)
    return dict(value)


def load_native(value, path, raw, *, cwd=None, workspace_binding=None):
    from store_registry import native_select
    from hooks import _bounded_json, MAX_CONFIG
    if workspace_binding is not None or cwd is None:
        raise ValueError("native hook selection requires event workspace, not legacy binding")
    config = validate_native_static(value)
    current = Path(cwd).absolute()
    canonical = current.resolve(strict=True)
    if not canonical.is_dir(): raise ValueError("native hook workspace must be a directory")
    matching = [row for row in config.get("workspace_adapters", []) if canonical.is_relative_to(Path(row["root"]).resolve(strict=True))]
    if matching:
        adapter = max(matching, key=lambda row: len(Path(row["root"]).parts))
        config.update({key: adapter[key] for key in ("state_dir", "reader_codex", "reader_codex_sha256")})
    selected = native_select(config["memory_config"], config["native_cli"], current)
    background = selected.snapshot["config"]["background"]
    # Model defaults are executable host recipe, not another policy parser.
    config.update(reader_model=background.get("model") or "gpt-6.1-sol",
                  librarian_effort=background.get("effort") or "medium",
                  memory_scope="registry", memory_mode="async", project_root=current,
                  selection_cwd=current, recall_mode="automatic" if selected.recallable else "off",
                  recording_mode="automatic" if selected.recordable else "off",
                  _background_maintenance=bool(selected.maintainable),
                  frozen_selection=selected.to_dict(), _config_path=Path(path).resolve(),
                  _registry_config_sha256=hashlib.sha256(raw).hexdigest())
    resolve_budget(config)
    for key in ("state_dir", "memory_config", "native_cli", "reader_codex", "reader_auth"):
        if key in config: config[key] = Path(config[key]).resolve()
    if _bounded_json(Path(path), MAX_CONFIG, with_raw=True)[1] != raw:
        raise ValueError("native hook adapter changed during selection")
    from recording_jobs import _config_digest
    config["_reader_config_pin"] = _config_digest(config)
    selection(config)
    return config
