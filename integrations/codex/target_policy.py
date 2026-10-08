"""One explicitly selected librarian store, resolved before any contact."""
from dataclasses import dataclass
from pathlib import Path
import re

from profile import select_for_cwd, permit_service
from service import ConnectConfig, load_config, server_database_path

ULID = re.compile(r"[0-7][0-9A-HJKMNP-TV-Z]{25}\Z")
WORKSHOP_SCHEMA = "mneme.codex-hooks.config.v9"
GLOBAL_PREFERENCES_SCHEMA = "mneme.codex-hooks.config.v10"
MISC_SCHEMA = "mneme.codex-hooks.config.v11"
GLOBAL_PREFERENCE_TAG = "collaboration-preference"
GLOBAL_PREFERENCE_NAMESPACE = "codex-global-preference.v1"
GLOBAL_PREFERENCE_CUE = "User cross-project collaboration preferences for working with an assistant."


@dataclass(frozen=True)
class TargetPolicy:
    scope: str
    db_alias: str
    database_path: str
    db_id: str | None
    workspace_root: str
    service_config: str
    workspace_origin: str | None = None
    excluded_roots: tuple[str, ...] = ()

    def __post_init__(self):
        if (self.scope not in ("project", "workshop", "global_preference", "misc")
                or self.db_alias != ("project" if self.scope in ("project", "misc") else "user")
                or not Path(self.workspace_root).is_absolute() or not Path(self.service_config).is_absolute()
                or self.scope != "project" and (not isinstance(self.db_id, str) or not ULID.fullmatch(self.db_id))):
            raise ValueError("invalid explicit librarian target")
        server_database_path(self.database_path)
        if self.scope == "misc":
            from misc_binding import validate_binding, validate_excluded_roots
            if not isinstance(self.excluded_roots, tuple):
                raise ValueError("misc exclusions must be frozen")
            validate_excluded_roots(self.excluded_roots)
            validate_binding({"workspace_root": self.workspace_root,
                              "workspace_origin": self.workspace_origin}, self.excluded_roots)
        elif self.workspace_origin is not None or self.excluded_roots:
            raise ValueError("workspace binding belongs only to misc scope")

    def canonical(self):
        value = {field: getattr(self, field) for field in self.__dataclass_fields__}
        if self.scope != "misc":
            value.pop("workspace_origin")
            value.pop("excluded_roots")
        else:
            value["excluded_roots"] = list(self.excluded_roots)
        return value

    def validate_workspace(self, cwd=None):
        root = Path(self.workspace_root)
        if self.scope == "misc":
            from misc_binding import choose_workspace, validate_binding
            validate_binding({"workspace_root": self.workspace_root,
                              "workspace_origin": self.workspace_origin}, self.excluded_roots)
            current = root if cwd is None else Path(cwd).resolve(strict=True)
            current.relative_to(root)
            if choose_workspace(root if cwd is None else cwd, self.excluded_roots) is None:
                raise ValueError("misc workspace is configured or excluded")
            return
        current = root if cwd is None else Path(cwd).resolve()
        current.relative_to(root)
        if self.scope == "project":
            return
        selection = select_for_cwd(current)
        if selection["mode"] == "isolated" or selection["configured"] and selection["root"] != root:
            raise ValueError("personal target forbids isolated or nested project selection")
        if self.scope == "global_preference":
            permit_service(selection, self.db_alias, self.database_path, self.service_config)
            return
        # A plain nested repo is a project boundary even without a profile.
        for parent in (current, *current.parents):
            if parent == root:
                break
            if any((parent / name).exists() for name in
                   (".git", ".mneme/profile.json", ".mneme/codex-memory.db")):
                raise ValueError("workshop target forbids nested project capture")
        permit_service(selection, self.db_alias, self.database_path, self.service_config)

    def validate_service(self, service):
        self.validate_workspace()
        if service.database_name != self.db_alias or str(service.database_path) != self.database_path:
            raise ValueError("explicit store target differs from service configuration")
        if not isinstance(service, ConnectConfig) and not service.database_path.is_file():
            raise ValueError("project store is unavailable" if self.scope == "project" else "selected store is unavailable")

    def catalog_identity(self, catalog, expected=None, *, require_id=True):
        if (not isinstance(catalog, list) or len(catalog) != 1 or not isinstance(catalog[0], dict)
                or catalog[0].get("db") != self.db_alias or catalog[0].get("name") != self.db_alias
                or catalog[0].get("state") != "open"
                or catalog[0].get("configured_path") != self.database_path):
            raise ValueError("unexpected explicit store catalog")
        identifier = catalog[0].get("db_id")
        pinned = self.db_id if expected is None else expected
        if self.db_id is not None and expected is not None and expected != self.db_id:
            raise ValueError("explicit database identity changed")
        if pinned is not None and identifier != pinned:
            raise ValueError("explicit database identity changed")
        if not isinstance(identifier, str) or not ULID.fullmatch(identifier):
            if require_id or pinned is not None:
                raise ValueError("catalog has no canonical database identity")
            return None
        return identifier


def policy_for(service_config_path, root, store_target=None):
    """Project defaults are unchanged; workshop accepts only a frozen policy."""
    root, path = Path(root), Path(service_config_path)
    if not root.is_absolute() or not path.is_absolute():
        raise ValueError("store target paths must be absolute")
    if store_target is not None:
        if (not isinstance(store_target, TargetPolicy) or store_target.scope not in ("workshop", "global_preference", "misc")
                or store_target.workspace_root != str(root)
                or store_target.service_config != str(path)):
            raise ValueError("invalid explicit librarian target")
        policy = store_target
        policy.validate_workspace()
    service = load_config(path)
    if store_target is None:
        if service.database_name != "project" or (not isinstance(service, ConnectConfig)
                and service.database_path != root / ".mneme" / "codex-memory.db"):
            raise ValueError("project store boundary mismatch")
        policy = TargetPolicy("project", "project", str(service.database_path), None, str(root), str(path))
    policy.validate_service(service)
    return policy, service


def workshop_policy(config):
    """Only v9 can authorize user-store automatic read/recording."""
    target = config.get("store_target")
    if (config.get("schema") != WORKSHOP_SCHEMA or config.get("memory_scope") != "workshop"
            or not isinstance(target, dict) or set(target) != {"db_alias", "database_path", "db_id"}
            or target["db_alias"] != "user" or not isinstance(target["db_id"], str)
            or not ULID.fullmatch(target["db_id"])):
        raise ValueError("workshop requires v9 explicit user store target")
    database = server_database_path(target["database_path"])
    policy = TargetPolicy("workshop", "user", database, target["db_id"],
                          str(config["project_root"]), str(config["service_config"]))
    policy_for(config["service_config"], config["project_root"], policy)
    return policy


def misc_policy(config):
    """A bound v11 device default selects only the pinned shared project store."""
    from misc_binding import validate_binding, validate_excluded_roots
    target = config.get("store_target")
    if (config.get("schema") != MISC_SCHEMA or config.get("memory_scope") != "misc"
            or not isinstance(target, dict) or set(target) != {"db_alias", "database_path", "db_id"}
            or target.get("db_alias") != "project" or not isinstance(target.get("db_id"), str)
            or not ULID.fullmatch(target["db_id"])):
        raise ValueError("misc requires v11 explicit shared project store target")
    exclusions = validate_excluded_roots(config.get("excluded_roots"))
    binding = validate_binding(config.get("workspace_binding"), exclusions)
    if str(config.get("project_root")) != binding["workspace_root"]:
        raise ValueError("misc project root differs from workspace binding")
    policy = TargetPolicy("misc", "project", server_database_path(target["database_path"]), target["db_id"],
                          binding["workspace_root"], str(config["service_config"]),
                          binding["workspace_origin"], exclusions)
    policy_for(config["service_config"], config["project_root"], policy)
    return policy


def validate_global_preferences(value):
    """Parse the explicit owner binding; this does not contact or open its store."""
    if (not isinstance(value, dict) or set(value) != {"service_config", "database_path", "db_id"}
            or not isinstance(value["service_config"], str)
            or "\x00" in value["service_config"] or len(value["service_config"].encode()) > 4096
            or not Path(value["service_config"]).is_absolute()
            or not isinstance(value["db_id"], str) or not ULID.fullmatch(value["db_id"])):
        raise ValueError("global_preferences requires an explicit service, database path and db_id")
    server_database_path(value["database_path"])
    return dict(value)


def global_preferences_policy(config):
    """Only the reviewed project opt-in can select the personal preference lane."""
    if "global_preferences" not in config:
        return None
    if (config.get("schema") != GLOBAL_PREFERENCES_SCHEMA
            or config.get("memory_scope", "project") != "project"
            or config.get("memory_mode") != "async"):
        raise ValueError("global_preferences requires schema v10 project async configuration")
    target = validate_global_preferences(config["global_preferences"])
    policy = TargetPolicy("global_preference", "user", target["database_path"], target["db_id"],
                          str(config["project_root"]), target["service_config"])
    policy_for(policy.service_config, config["project_root"], policy)
    return policy


def target_service_config(config, destination=None):
    if destination == "global_preference":
        policy = global_preferences_policy(config)
        if policy is None:
            raise ValueError("global preference target is not enabled")
        return Path(policy.service_config)
    if destination not in (None, "project"):
        raise ValueError("unknown recording destination")
    return Path(config["service_config"])


def target_kwargs(config, *, destination=None):
    # Preserve exact default call signatures for shipped project callers/jobs.
    if destination == "global_preference":
        policy = global_preferences_policy(config)
        if policy is None:
            raise ValueError("global preference target is not enabled")
        return {"store_target": policy}
    if destination not in (None, "project"):
        raise ValueError("unknown recording destination")
    if config.get("memory_scope") == "misc":
        return {"store_target": misc_policy(config)}
    return {"store_target": workshop_policy(config)} if config.get("memory_scope") == "workshop" else {}


def alias_for(config):
    if config.get("memory_scope") == "misc":
        return misc_policy(config).db_alias
    return workshop_policy(config).db_alias if config.get("memory_scope") == "workshop" else "project"
