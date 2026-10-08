"""Bounded, filesystem-only admission of genuinely unconfigured workspaces."""
import json
import os
from pathlib import Path
import shlex
import stat
import tomllib

from profile import MAX_ANCESTORS, MAX_PATH_BYTES, select_for_cwd

MAX_CONFIG_BYTES = 64 * 1024
MAX_EXCLUDED_ROOTS = 64
MAX_EXCLUDED_BYTES = 16 * 1024


def _absolute_path(value):
    if (not isinstance(value, (str, Path)) or not str(value) or "\x00" in str(value)
            or len(str(value).encode()) > MAX_PATH_BYTES or not Path(value).is_absolute()):
        raise ValueError("workspace path must be bounded and absolute")
    return Path(os.path.normpath(str(value)))


def canonical_path(value, *, directory=False):
    path = _absolute_path(value)
    for component in (path, *path.parents):
        if component.is_symlink():
            raise ValueError("workspace path must not traverse symlinks")
    canonical = path.resolve(strict=directory)
    if directory and not canonical.is_dir():
        raise ValueError("workspace must be an existing directory")
    return canonical


def validate_excluded_roots(values):
    if (not isinstance(values, (list, tuple)) or len(values) > MAX_EXCLUDED_ROOTS
            or any(not isinstance(value, str) for value in values)
            or sum(len(value.encode()) for value in values) > MAX_EXCLUDED_BYTES):
        raise ValueError("excluded_roots must be a bounded explicit path list")
    roots = tuple(str(_absolute_path(value)) for value in values)
    if any(original != root for original, root in zip(values, roots)) or len(set(roots)) != len(roots):
        raise ValueError("excluded_roots must be unique normalized absolute paths")
    return roots


def _unique(pairs):
    result = {}
    for key, value in pairs:
        if key in result:
            raise ValueError("duplicate workspace configuration field")
        result[key] = value
    return result


def _read(path):
    if path.is_symlink():
        raise ValueError("workspace configuration must not be a symlink")
    try:
        metadata = path.stat()
    except FileNotFoundError:
        return None
    if not stat.S_ISREG(metadata.st_mode) or metadata.st_size > MAX_CONFIG_BYTES:
        raise ValueError("workspace configuration must be a bounded regular file")
    with path.open("rb") as stream:
        raw = stream.read(MAX_CONFIG_BYTES + 1)
    if len(raw) > MAX_CONFIG_BYTES:
        raise ValueError("workspace configuration exceeds size limit")
    return raw


def _owned(value, depth=0):
    if depth > 32:
        raise ValueError("workspace configuration exceeds nesting limit")
    if isinstance(value, dict):
        return any(_owned(key, depth + 1) or _owned(item, depth + 1) for key, item in value.items())
    if isinstance(value, list):
        return any(_owned(item, depth + 1) for item in value)
    if not isinstance(value, str):
        return False
    if "mneme" in value.lower():
        return True
    # Historical copied launchers need not retain a Mneme-named parent path.
    try:
        words = shlex.split(value)
    except ValueError:
        words = [value]
    return any(Path(word).name in ("launcher.py", "library_launcher.py", "hooks.py", "hook_launcher.py")
               for word in words)


def _hook_events_owned(events, *, allow_state=False):
    if not isinstance(events, dict):
        raise ValueError("unknown workspace hooks configuration")
    configured = False
    for name, groups in events.items():
        # Codex hook trust/bookkeeping is not a runnable handler or enrollment.
        if allow_state and name == "state":
            continue
        if allow_state and name == "enabled":
            if not isinstance(groups, bool):
                raise ValueError("invalid workspace hooks enabled flag")
            continue
        if allow_state and name in ("managed_dir", "windows_managed_dir"):
            if (not isinstance(groups, str) or "\x00" in groups
                    or len(groups.encode()) > MAX_PATH_BYTES):
                raise ValueError("invalid workspace managed hook directory")
            # An externally managed handler set is an explicit boundary. Do not
            # discover or follow it just to decide whether misc is eligible.
            configured = bool(groups) or configured
            continue
        if not isinstance(groups, list):
            raise ValueError("invalid workspace hook event")
        for group in groups:
            if not isinstance(group, dict) or not isinstance(group.get("hooks"), list):
                raise ValueError("invalid workspace hook group")
            for handler in group["hooks"]:
                if not isinstance(handler, dict):
                    raise ValueError("invalid workspace hook handler")
                kind = handler.get("type")
                if kind == "command":
                    if not isinstance(handler.get("command"), str) or not handler["command"]:
                        raise ValueError("invalid workspace hook command")
                    configured = _owned(handler["command"]) or configured
                elif kind == "mcp_tool":
                    if any(not isinstance(handler.get(field), str) or not handler[field]
                           for field in ("server", "tool")):
                        raise ValueError("invalid workspace MCP hook route")
                    configured = ("mneme" in handler["server"].lower()
                                  or "mneme" in handler["tool"].lower() or configured)
                elif kind in ("prompt", "agent"):
                    if not isinstance(handler.get("prompt"), str) or not handler["prompt"]:
                        raise ValueError("invalid workspace prompt hook")
                    # Authored prompt/status text is not executable routing.
                else:
                    raise ValueError("unknown workspace hook handler type")
    return configured


def _codex_configured(directory):
    # The device hook is itself in the user config; it is not project enrollment.
    user_config = Path(os.environ.get("CODEX_HOME", str(Path.home() / ".codex"))).resolve()
    if directory.is_symlink():
        raise ValueError("workspace .codex directory must not be a symlink")
    if directory.resolve() == user_config:
        return False
    raw = _read(directory / "config.toml")
    configured = False
    if raw is not None:
        data = tomllib.loads(raw.decode("utf-8"))
        servers = data.get("mcp_servers", {})
        if not isinstance(servers, dict) or any(not isinstance(server, dict) for server in servers.values()):
            raise ValueError("invalid workspace MCP configuration")
        for name, server in servers.items():
            if (bool(server.get("command")) == bool(server.get("url"))):
                raise ValueError("workspace MCP endpoint requires one command or URL")
            for field in ("command", "url"):
                if field in server and not isinstance(server[field], str):
                    raise ValueError("invalid workspace MCP endpoint")
            if "args" in server and (not isinstance(server["args"], list)
                                     or any(not isinstance(arg, str) for arg in server["args"])):
                raise ValueError("invalid workspace MCP arguments")
            configured = configured or "mneme" in name.lower() or _owned(
                {field: server[field] for field in ("command", "args") if field in server})
        # Only routing/command surfaces establish ownership. Arbitrary user
        # instructions mentioning Mneme are not enrollment.
        if "hooks" in data:
            configured = _hook_events_owned(data["hooks"], allow_state=True) or configured
        if "notify" in data:
            if not isinstance(data["notify"], list) or any(not isinstance(arg, str) for arg in data["notify"]):
                raise ValueError("invalid workspace notify command")
            configured = _owned(data["notify"]) or configured
        configured = configured or b"# BEGIN mneme-codex" in raw
    raw = _read(directory / "hooks.json")
    if raw is not None:
        data = json.loads(raw, object_pairs_hook=_unique)
        if not isinstance(data, dict):
            raise ValueError("invalid workspace hooks configuration")
        if isinstance(data.get("schema"), str) and data["schema"].startswith("mneme."):
            return True  # Legacy hook configs are enrollment, never misc.
        configured = _hook_events_owned(data.get("hooks")) or configured
    return configured


def choose_workspace(cwd, excluded_roots=()):
    """Return an ephemeral binding or None; never load/contact a service."""
    origin = _absolute_path(cwd)
    current = origin.resolve(strict=True)
    if not current.is_dir():
        raise ValueError("workspace must be an existing directory")
    exclusions = validate_excluded_roots(excluded_roots)
    if any(candidate == excluded or excluded in candidate.parents
           for root in exclusions for candidate in (origin, current)
           for excluded in (Path(root), Path(root).resolve())):
        return None
    selection = select_for_cwd(current)  # Malformed profiles fail closed.
    if selection["configured"]:
        return None
    seen = set()
    for chain in (origin, current):
        for index, parent in enumerate((chain, *chain.parents)):
            if index >= MAX_ANCESTORS:
                raise ValueError("misc workspace search exceeds ancestor limit")
            if parent in seen:
                continue
            seen.add(parent)
            marker = parent / ".mneme"
            if marker.is_symlink():
                raise ValueError("workspace .mneme boundary must not be a symlink")
            if marker.exists():
                return None  # Even an empty owner directory is not unconfigured.
            if _codex_configured(parent / ".codex"):
                return None
    # Origin preserves lexical privacy ancestry; root is the resolved workspace.
    return {"workspace_root": str(current), "workspace_origin": str(origin)}


def validate_binding(binding, excluded_roots=()):
    if (not isinstance(binding, dict) or set(binding) != {"workspace_root", "workspace_origin"}
            or any(not isinstance(value, str) for value in binding.values())):
        raise ValueError("invalid misc workspace binding")
    root = _absolute_path(binding["workspace_root"])
    origin = _absolute_path(binding["workspace_origin"])
    if (str(root) != binding["workspace_root"] or str(origin) != binding["workspace_origin"]
            or root.resolve(strict=True) != root or origin.resolve(strict=True) != root):
        raise ValueError("misc workspace binding must retain its lexical origin and canonical root")
    selected = choose_workspace(origin, excluded_roots)
    if selected != binding:
        raise ValueError("misc workspace is configured or excluded")
    return dict(binding)
