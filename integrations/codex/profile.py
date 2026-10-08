"""Fail-closed, filesystem-only Codex project memory selection.

This module must be safe to import before any Mneme service configuration is read.
An absent profile is the historical standalone integration, not enrollment.
"""

import json
from pathlib import Path

SCHEMA = "mneme.profile.v1"
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
        if path.is_symlink():
            raise ValueError("project profile must not be a symlink")
        with path.open("rb") as stream:
            raw = stream.read(MAX_PROFILE_BYTES + 1)
    except FileNotFoundError:
        return {"mode": "default", "library_config": None,
                "configured": False, "root": root}
    if len(raw) > MAX_PROFILE_BYTES:
        raise ValueError("project profile exceeds size limit")
    value = json.loads(raw, object_pairs_hook=_unique)
    if (not isinstance(value, dict) or value.get("schema") != SCHEMA
            or set(value) not in ({"schema", "mode"}, {"schema", "mode", "library_config"})
            or value["mode"] not in ("default", "private", "isolated")):
        raise ValueError("invalid project profile")
    library = _absolute(value["library_config"]) if "library_config" in value else None
    return {"mode": value["mode"], "library_config": library,
            "configured": True, "root": root}


def select_for_cwd(cwd):
    """Find the nearest explicit profile; never contact a personal service here."""
    if not isinstance(cwd, (str, Path)) or not Path(cwd).is_absolute():
        raise ValueError("working directory must be absolute")
    current = Path(cwd).resolve()
    for index, parent in enumerate((current, *current.parents)):
        if index >= MAX_ANCESTORS:
            raise ValueError("project profile search exceeds ancestor limit")
        path = parent / ".mneme" / "profile.json"
        if path.exists() or path.is_symlink():
            return load_profile(parent)
    return {"mode": "default", "library_config": None,
            "configured": False, "root": None}


def permit_service(selection, database_name, database_path, service_config_path=None):
    """Enforce process-level profile routing before host startup or connection."""
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
    with path.open("rb") as stream:
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
