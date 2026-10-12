"""Fail-closed, filesystem-only Codex project memory selection.

This module must be safe to import before any Mneme service configuration is read.
An absent profile is the historical standalone integration, not enrollment.
"""

import json
import os
from pathlib import Path

from host_files import open_regular

SCHEMA = "mneme.profile.v1"
SCHEMA_V2 = "mneme.profile.v2"
MAX_PROFILE_BYTES = 8192
MAX_PATH_BYTES = 4096
MAX_ANCESTORS = 64
MAX_LIBRARY_BYTES = 64 * 1024


def _unique(pairs):
    result = {}
    for key, value in pairs:
        if key in result:
            raise ValueError("duplicate project profile field")
        result[key] = value
    return result


def _absolute(value):
    if (not isinstance(value, str) or not value or "\x00" in value
            or len(value.encode("utf-8")) > MAX_PATH_BYTES or not Path(value).is_absolute()):
        raise ValueError("library_config must be a bounded absolute path")
    return Path(value)


def load_profile(project_root):
    """Return a validated selection, with `configured=False` for legacy projects."""
    root = Path(project_root)
    if not root.is_absolute():
        raise ValueError("project root must be absolute")
    path = root / ".mneme" / "profile.json"
    try:
        if path.is_symlink() or path.parent.is_symlink():
            raise ValueError("project profile must not be a symlink")
        with os.fdopen(open_regular(path), "rb") as stream:
            raw = stream.read(MAX_PROFILE_BYTES + 1)
    except FileNotFoundError:
        return {"mode": "default", "library_config": None,
                "configured": False, "root": root, "stores": None}
    if len(raw) > MAX_PROFILE_BYTES:
        raise ValueError("project profile exceeds size limit")
    value = json.loads(raw, object_pairs_hook=_unique)
    if (not isinstance(value, dict) or value.get("schema") not in (SCHEMA, SCHEMA_V2)
            or not {"schema", "mode"} <= set(value)
            or set(value) - {"schema", "mode", "library_config", "stores"}
            or value["mode"] not in ("default", "private", "isolated")):
        raise ValueError("invalid project profile")
    if value["schema"] == SCHEMA and "stores" in value:
        raise ValueError("named stores require project profile v2")
    stores = value.get("stores")
    if "stores" in value:
        import re
        if (not isinstance(stores, list) or any(not isinstance(name, str)
                or not re.fullmatch(r"[A-Za-z0-9_.-]{1,128}", name) for name in stores)
                or len(set(stores)) != len(stores)):
            raise ValueError("profile stores must be a unique named-store allowlist")
        stores = tuple(stores)
    library = _absolute(value["library_config"]) if "library_config" in value else None
    if stores is not None and library is not None:
        raise ValueError("profile must select named stores or a legacy library, not both")
    return {"mode": value["mode"], "library_config": library,
            "configured": True, "root": root, "stores": stores}



def select_for_cwd(cwd):
    """Find the nearest explicit profile; never contact a personal service here."""
    if not isinstance(cwd, (str, Path)) or not Path(cwd).is_absolute():
        raise ValueError("working directory must be absolute")
    origin = Path(os.path.abspath(cwd))
    candidates = []
    # A profile reached *through* a symlink cannot erase the lexical privacy
    # boundary outside that link. Inspect each link's parent separately from
    # the canonical destination (including harmless platform aliases /var).
    spellings = [origin.resolve()]
    for index, ancestor in enumerate((origin, *origin.parents)):
        if index >= MAX_ANCESTORS:
            raise ValueError("project profile search exceeds ancestor limit")
        if ancestor.is_symlink():
            spellings.append(ancestor.parent)
    for current in dict.fromkeys(spellings):
        for index, parent in enumerate((current, *current.parents)):
            if index >= MAX_ANCESTORS:
                raise ValueError("project profile search exceeds ancestor limit")
            marker = parent / ".mneme"
            if marker.is_symlink():
                raise ValueError("project profile directory must not be a symlink")
            path = marker / "profile.json"
            try:
                path.lstat()
            except FileNotFoundError:
                continue
            candidates.append(load_profile(parent))
            break
    if candidates:
        chosen = candidates[0]
        for candidate in candidates[1:]:
            if ({**candidate, "root": candidate["root"].resolve()}
                    != {**chosen, "root": chosen["root"].resolve()}):
                raise ValueError("lexical and canonical project profiles disagree")
        return {**chosen, "root": chosen["root"].resolve()}
    return {"mode": "default", "library_config": None,
            "configured": False, "root": None, "stores": None}


def permit_service(selection, database_name, database_path, service_config_path=None, *, selected_store=None, registry_selection=None):
    """Enforce process-level profile routing before host startup or connection."""
    if selected_store is not None or registry_selection is not None:
        from store_registry import SelectedStore, revalidate
        if not isinstance(selected_store, SelectedStore) or registry_selection is None:
            raise ValueError("arbitrary database aliases require a checked selected binding")
        current = revalidate(registry_selection, store_name=selected_store.name)
        actual = current.named(selected_store.name)
        if (actual != selected_store or database_name != actual.db_alias
                or str(database_path) != actual.database_path
                or service_config_path is None or str(service_config_path) != actual.service_config
                or dict(current.profile) != {key: str(value) if isinstance(value, Path) else value
                                              for key, value in selection.items()}):
            raise ValueError("service differs from checked selected binding")
        return
    if database_name not in ("user", "project"):
        raise ValueError("arbitrary database aliases require a checked selected binding")
    if selection.get("stores") is not None and database_name == "user":
        raise ValueError("named-store profile requires a checked selected binding")
    if selection["mode"] == "isolated" and database_name == "user":
        raise ValueError("isolated project forbids personal memory service")
    if database_name == "user" and selection["library_config"] is not None:
        core = library_core(selection)
        if (core is None or Path(database_path) != core["global_database"]
                or service_config_path is None
                or Path(service_config_path) != core["global_service"]):
            raise ValueError("selected library forbids ambient personal memory service")
    if selection["mode"] == "isolated" and database_name == "project":
        expected = selection["root"] / ".mneme" / "codex-memory.db"
        if Path(database_path) != expected:
            raise ValueError("isolated project forbids another project store")


def library_core(selection):
    """Read only the explicitly selected library's core routing metadata."""
    path = selection["library_config"]
    if path is None:
        return None
    if path.is_symlink():
        raise ValueError("library config must not be a symlink")
    with os.fdopen(open_regular(path), "rb") as stream:
        raw = stream.read(MAX_LIBRARY_BYTES + 1)
    if len(raw) > MAX_LIBRARY_BYTES:
        raise ValueError("library config exceeds size limit")
    value = json.loads(raw, object_pairs_hook=_unique)
    if not isinstance(value, dict) or value.get("schema") != "mneme.library.config.v1":
        raise ValueError("invalid selected library config")
    core = value.get("core")
    if core is None:
        return None
    if not isinstance(core, dict) or set(core) != {"global_service", "global_database"}:
        raise ValueError("invalid selected library core config")
    return {"global_service": _absolute(core["global_service"]),
            "global_database": _absolute(core["global_database"])}
