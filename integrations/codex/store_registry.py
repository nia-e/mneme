"""Bounded, filesystem-only selection of explicitly configured memory owners.

Names describe policy, aliases route requests, and native IDs identify owners.
Parsing and admission never start an owner or open its database. Legacy config is
not a fallback: callers opt into this registry or retain their legacy adapter.
"""
from dataclasses import asdict, dataclass, field, replace
import hashlib
import json
import os
from pathlib import Path
import re

from host_files import open_regular
from host_platform import default_config_dir
from profile import select_for_cwd
from service import load_config, server_database_path

SCHEMA = "mneme.stores.v1"
MAX_REGISTRY_BYTES = 64 * 1024
MAX_PURPOSE_BYTES = 2048
MAX_SERVICE_BYTES = 32768
MAX_SERVICE_SNAPSHOT_BYTES = 256 * 1024
MAX_PATH_BYTES = 4096
NAME = re.compile(r"[A-Za-z0-9_.-]{1,128}\Z")
ULID = re.compile(r"[0-7][0-9A-HJKMNP-TV-Z]{25}\Z")
PROFILES = ("default", "private", "isolated")
KINDS = ("lesson", "episode", "possibility")
EVIDENCE_KINDS = ("user_statement", "assistant_assertion", "tool_call", "tool_result", "memory_delivery")


def _digest(value):
    return hashlib.sha256(json.dumps(value, sort_keys=True, separators=(",", ":"),
                                     ensure_ascii=False).encode()).hexdigest()


def _unique(pairs):
    result = {}
    for key, value in pairs:
        if key in result:
            raise ValueError("duplicate store registry field")
        result[key] = value
    return result


def _read(path, limit):
    with os.fdopen(open_regular(path), "rb") as stream:
        raw = stream.read(limit + 1)
    if len(raw) > limit:
        raise ValueError("store registry input exceeds byte limit")
    return raw


def _object(value, fields, required=()):
    if not isinstance(value, dict) or set(value) - set(fields) or not set(required) <= set(value):
        raise ValueError("invalid or unknown store registry fields")
    return value


def _text(value, limit, *, empty=False):
    if (not isinstance(value, str) or "\x00" in value or not empty and not value.strip()
            or len(value.encode("utf-8")) > limit):
        raise ValueError("invalid bounded store registry text")
    return value


def _bool(value):
    if type(value) is not bool:
        raise ValueError("store permissions must be explicit booleans")
    return value


def _strings(value, *, allowed=None):
    if not isinstance(value, (list, tuple)):
        raise ValueError("store policy list must be an array")
    result = tuple(_text(item, 256) for item in value)
    if len(set(result)) != len(result) or allowed is not None and not set(result) <= set(allowed):
        raise ValueError("duplicate or unsupported store policy value")
    return result


def _path(value, *, base=None):
    value = _text(str(value) if isinstance(value, Path) else value, MAX_PATH_BYTES)
    path = Path(value)
    if not path.is_absolute():
        if base is None:
            raise ValueError("store path must be absolute")
        path = base / path
    return Path(os.path.abspath(path))


def database_path_identity(value):
    # Remote paths are opaque server identity, never client filesystem access.
    # Native Windows managed owners instead retain their own absolute path form.
    try:
        return server_database_path(value)
    except ValueError:
        if (isinstance(value, str) and Path(value).is_absolute()
                and str(Path(os.path.normpath(value))) == value):
            return _text(value, MAX_PATH_BYTES)
        raise


def _paths(value):
    if not isinstance(value, (list, tuple)):
        raise ValueError("workspace roots must be an array")
    result = tuple(str(_path(item)) for item in value)
    if len(set(result)) != len(result):
        raise ValueError("duplicate workspace root")
    return result


@dataclass(frozen=True)
class ReadPolicy:
    enabled: bool = False
    core: bool = False
    cue: str | None = None


@dataclass(frozen=True)
class RecordPolicy:
    enabled: bool = False
    kinds: tuple[str, ...] = ()
    evidence_kinds: tuple[str, ...] = ()
    required_tags: tuple[str, ...] = ()
    allow_associations: bool = False
    max_summary_bytes: int = 384
    max_body_bytes: int = 1024


@dataclass(frozen=True)
class MaintenancePolicy:
    enabled: bool = False
    tag_guide_id: str | None = None


@dataclass(frozen=True)
class Applicability:
    workspace: str = "any"
    roots: tuple[str, ...] = ()
    excluded_roots: tuple[str, ...] = ()
    profiles: tuple[str, ...] = ("default", "private")


def _policies(value):
    when = _object(value.get("when", {}), ("workspace", "roots", "excluded_roots", "profiles"))
    workspace = when.get("workspace", "any")
    if workspace not in ("any", "configured", "unconfigured"):
        raise ValueError("invalid store workspace admission")
    applies = Applicability(workspace, _paths(when.get("roots", [])),
                            _paths(when.get("excluded_roots", [])),
                            _strings(when.get("profiles", ["default", "private"]), allowed=PROFILES))
    read = _object(value.get("read", {}), ("enabled", "core", "cue"))
    cue = read.get("cue")
    if cue is not None:
        cue = _text(cue, MAX_PURPOSE_BYTES)
    reading = ReadPolicy(_bool(read.get("enabled", False)), _bool(read.get("core", False)), cue)
    if reading.core and not reading.enabled:
        raise ValueError("core requires read permission")
    record = _object(value.get("record", {}), ("enabled", "kinds", "evidence_kinds", "required_tags",
                                               "allow_associations", "max_summary_bytes", "max_body_bytes"))
    summary, body = record.get("max_summary_bytes", 384), record.get("max_body_bytes", 1024)
    if type(summary) is not int or not 1 <= summary <= 512 or type(body) is not int or not 1 <= body <= 2048:
        raise ValueError("record byte limits exceed native authored-note envelope")
    recording = RecordPolicy(_bool(record.get("enabled", False)),
                             _strings(record.get("kinds", []), allowed=KINDS),
                             _strings(record.get("evidence_kinds", []), allowed=EVIDENCE_KINDS),
                             _strings(record.get("required_tags", [])),
                             _bool(record.get("allow_associations", False)), summary, body)
    if set(recording.required_tags) & {"core", "routing-judgment", "possibility", "pursuing", "closed"}:
        raise ValueError("record policy cannot grant core, feedback or lifecycle tags")
    if len(recording.required_tags) > 32:
        raise ValueError("required tags exceed native capture limit")
    from tag_context import ordinary_tags
    ordinary_tags([tag for tag in recording.required_tags if tag != "collaboration-preference"], max_count=32)
    if recording.enabled and (not recording.kinds or not recording.evidence_kinds):
        raise ValueError("record permission requires explicit kinds and evidence kinds")
    maintenance = _object(value.get("maintenance", {}), ("enabled", "tag_guide_id"))
    guide = maintenance.get("tag_guide_id")
    if guide is not None and (not isinstance(guide, str) or not ULID.fullmatch(guide)):
        raise ValueError("tag guide must have a native ID")
    upkeep = MaintenancePolicy(_bool(maintenance.get("enabled", False)), guide)
    return applies, reading, recording, upkeep


@dataclass(frozen=True)
class SelectedStore:
    name: str
    service_config: str
    db_alias: str
    database_path: str
    db_id: str | None
    purpose: str
    when: Applicability = Applicability()
    read: ReadPolicy = ReadPolicy()
    record: RecordPolicy = RecordPolicy()
    maintenance: MaintenancePolicy = MaintenancePolicy()
    service_digest: str = ""
    workspace_root: str | None = None
    _service: object = field(default=None, compare=False, repr=False)

    @property
    def is_project(self):
        return self.workspace_root is not None

    @property
    def owner_key(self):
        """Stable accounting identity; display aliases never mint new budgets."""
        return self.db_id

    @property
    def digest(self):
        return _digest(self.to_dict())

    def qualify(self, node_id):
        return (self.db_id, node_id)

    def to_dict(self):
        return {"name": self.name, "service_config": self.service_config,
                "db_alias": self.db_alias, "database_path": self.database_path,
                "db_id": self.db_id, "purpose": self.purpose, "when": asdict(self.when),
                "read": asdict(self.read), "record": asdict(self.record),
                "maintenance": asdict(self.maintenance), "service_digest": self.service_digest,
                "workspace_root": self.workspace_root}

    def service(self):
        """Return the already parsed snapshot only while its file is unchanged."""
        if self._service is None or hashlib.sha256(_read(self.service_config, MAX_SERVICE_BYTES)).hexdigest() != self.service_digest:
            raise ValueError("selected service configuration changed; select again")
        return self._service


_ENTRY_FIELDS = ("name", "service_config", "db_alias", "database_path", "db_id", "purpose",
                 "when", "read", "record", "maintenance")


def _entry(value, base, *, project=False):
    fields = _ENTRY_FIELDS + (("workspace_root", "service_digest") if project else ())
    _object(value, fields, _ENTRY_FIELDS[:6])
    if not isinstance(value["name"], str) or not NAME.fullmatch(value["name"]):
        raise ValueError("invalid logical store name")
    if not isinstance(value["db_alias"], str) or not NAME.fullmatch(value["db_alias"]):
        raise ValueError("invalid native database alias")
    identifier = value["db_id"]
    if not (project and identifier is None) and (not isinstance(identifier, str) or not ULID.fullmatch(identifier)):
        raise ValueError("store requires a pinned native database identity")
    policies = _policies(value)
    root = str(_path(value["workspace_root"])) if project else None
    return SelectedStore(value["name"], str(_path(value["service_config"], base=base)), value["db_alias"],
                         database_path_identity(value["database_path"]), identifier,
                         _text(value["purpose"], MAX_PURPOSE_BYTES, empty=project), *policies,
                         workspace_root=root)


def _bind(store, budget=None):
    # Denied entries never reach here. Read twice around the legacy parser to
    # refuse ordinary concurrent edits instead of freezing mixed generations.
    before = _read(store.service_config, MAX_SERVICE_BYTES)
    if budget is not None:
        budget[0] += len(before)
        if budget[0] > MAX_SERVICE_SNAPSHOT_BYTES:
            raise ValueError("selected service configurations exceed aggregate byte limit")
    service = load_config(store.service_config)
    if before != _read(store.service_config, MAX_SERVICE_BYTES):
        raise ValueError("service configuration changed during selection")
    if service.database_name != store.db_alias or str(service.database_path) != store.database_path:
        raise ValueError("registry owner differs from selected service configuration")
    return replace(store, service_digest=hashlib.sha256(before).hexdigest(), _service=service)


@dataclass(frozen=True)
class Registry:
    path: str
    digest: str
    stores: tuple[SelectedStore, ...]


def default_registry_path():
    return default_config_dir() / "stores.json"


def load_registry(path=None):
    path = _path(default_registry_path() if path is None else path)
    raw = _read(path, MAX_REGISTRY_BYTES)  # Missing is an error, not a legacy fallback.
    try:
        value = json.loads(raw, object_pairs_hook=_unique)
    except (UnicodeError, RecursionError) as error:
        raise ValueError("invalid store registry encoding or nesting") from error
    _object(value, ("schema", "stores"), ("schema", "stores"))
    if value["schema"] != SCHEMA or not isinstance(value["stores"], list):
        raise ValueError("invalid store registry schema")
    stores = tuple(_entry(entry, path.parent) for entry in value["stores"])
    if len({store.name for store in stores}) != len(stores):
        raise ValueError("duplicate logical store name")
    if len({store.db_id for store in stores}) != len(stores):
        raise ValueError("duplicate native owner identity")
    return Registry(str(path), hashlib.sha256(raw).hexdigest(), stores)


def _inside(path, roots, *, include=False):
    matches = [any(candidate == root or root in candidate.parents
                   for value in roots for root in (Path(value), Path(value).resolve()))
               for candidate in (path, path.resolve())]
    # Positive admission needs both spellings inside the selected workspace.
    # Exclusion wins if either spelling crosses an excluded ancestry.
    return all(matches) if include else any(matches)


def _profile_snapshot(profile):
    return tuple(sorted((key, tuple(value) if isinstance(value, list) else str(value) if isinstance(value, Path) else value)
                        for key, value in profile.items()))


def _admitted(store, cwd, profile, *, unconfigured):
    allowlist = profile.get("stores")
    if allowlist is not None and store.name not in allowlist:
        return "profile allowlist"
    if profile["mode"] == "isolated" and allowlist is None:
        return "isolated profile"
    if profile["library_config"] is not None and allowlist is None:
        return "explicit legacy library"
    if profile["mode"] not in store.when.profiles:
        return "profile mode"
    if store.when.roots and not _inside(cwd, store.when.roots, include=True):
        return "workspace roots"
    if _inside(cwd, store.when.excluded_roots):
        return "excluded workspace"
    if store.when.workspace == "configured" and unconfigured:
        return "unconfigured workspace"
    if store.when.workspace == "unconfigured" and not unconfigured:
        return "configured workspace"
    return None


@dataclass(frozen=True)
class Selection:
    registry_path: str
    registry_digest: str
    cwd: str
    mode: str
    profile: tuple
    stores: tuple[SelectedStore, ...]
    omissions: tuple[tuple[str, str], ...] = ()
    project_binding: SelectedStore | None = None
    canonical_cwd: str = ""

    @property
    def core(self):
        return tuple(store for store in self.stores if store.read.enabled and store.read.core) if self.mode != "off" else ()

    @property
    def readable(self):
        return tuple(store for store in self.stores if store.read.enabled) if self.mode != "off" else ()

    @property
    def recordable(self):
        return tuple(store for store in self.stores if store.record.enabled) if self.mode == "async" else ()

    @property
    def maintainable(self):
        return tuple(store for store in self.stores if store.maintenance.enabled) if self.mode == "async" else ()

    @property
    def digest(self):
        return _digest({"registry": self.registry_digest, "cwd": self.cwd, "canonical_cwd": self.canonical_cwd, "mode": self.mode,
                        "profile": self.profile, "stores": [store.to_dict() for store in self.stores],
                        "omissions": self.omissions})

    def named(self, name):
        for store in self.stores:
            if store.name == name:
                return store
        raise ValueError("store is not selected")

    def to_dict(self):
        return {"schema": "mneme.store-selection.v1", "registry_path": self.registry_path,
                "cwd": self.cwd, "mode": self.mode, "digest": self.digest,
                "project_binding": self.project_binding.to_dict() if self.project_binding else None}

    @classmethod
    def from_dict(cls, value):
        _object(value, ("schema", "registry_path", "cwd", "mode", "digest", "project_binding"),
                ("schema", "registry_path", "cwd", "mode", "digest", "project_binding"))
        if value["schema"] != "mneme.store-selection.v1":
            raise ValueError("invalid selected store snapshot")
        project = value["project_binding"]
        if project is not None:
            project = _entry(project, None, project=True)
        current = resolve(value["cwd"], registry_path=value["registry_path"],
                          project_binding=project, mode=value["mode"])
        if current.digest != value["digest"]:
            raise ValueError("selected store admission changed; reassess queued work")
        return current


def _project_marker(parent):
    for name in (".mneme", ".git"):
        try:
            (parent / name).lstat()
        except FileNotFoundError:
            continue
        return True
    return False


def admit_project_workspace(cwd, root, profile=None):
    """Shared pure project admission; call before parsing its service config."""
    cwd, root = _path(cwd), _path(root)
    current = cwd.resolve(strict=True)
    if not current.is_dir() or not current.is_relative_to(root.resolve()):
        raise ValueError("project binding does not contain workspace")
    profile = select_for_cwd(cwd) if profile is None else profile
    if profile["configured"] and profile["root"].resolve() != root.resolve():
        raise ValueError("nested profile refuses inherited project binding")
    for spelling in dict.fromkeys((cwd, current)):
        ancestry = (spelling, *spelling.parents)
        # A nested alias may point back to the project root. It must not make
        # us stop before seeing its lexical enclosing project.
        boundaries = [parent for parent in ancestry if parent.resolve() == root.resolve()]
        boundary = boundaries[-1] if boundaries else None
        for parent in ancestry:
            if parent == boundary:
                break
            if parent.resolve() == root.resolve():
                continue  # This is the selected owner's own marker via an alias.
            if _project_marker(parent):
                raise ValueError("nested project refuses inherited project binding")


def select(registry, cwd, *, project_binding=None, mode="async"):
    if not isinstance(registry, Registry) or mode not in ("async", "read", "reminder", "off"):
        raise ValueError("invalid store selection")
    cwd = _path(cwd)
    profile = select_for_cwd(cwd)  # Validate profile before reading any service.
    from misc_binding import choose_workspace
    unconfigured = choose_workspace(cwd) is not None
    stores, omissions, service_bytes = [], [], [0]
    def eligible(store):
        return (mode != "off" and (store.read.enabled or mode == "async"
                and (store.record.enabled or store.maintenance.enabled)))
    if project_binding is not None:
        if not isinstance(project_binding, SelectedStore) or not project_binding.is_project:
            raise ValueError("project injection requires a checked project binding")
        admit_project_workspace(cwd, project_binding.workspace_root, profile)
        if eligible(project_binding):
            stores.append(_bind(project_binding, service_bytes))
        else:
            omissions.append((project_binding.name, "no enabled operations"))
        unconfigured = False
    for store in registry.stores:
        reason = _admitted(store, cwd, profile, unconfigured=unconfigured)
        if reason is None and not eligible(store):
            reason = "no enabled operations"
        if reason is not None:
            omissions.append((store.name, reason))
            continue
        if any(prior.name == store.name or prior.db_id is not None and prior.db_id == store.db_id for prior in stores):
            raise ValueError("registry entry conflicts with injected project owner")
        stores.append(_bind(store, service_bytes))
    return Selection(registry.path, registry.digest, str(cwd), mode, _profile_snapshot(profile),
                     tuple(stores), tuple(omissions), project_binding, str(cwd.resolve()))


def resolve(cwd, *, registry_path=None, project_binding=None, mode="async"):
    # Invalid profiles must fail before even an ambient registry is inspected.
    select_for_cwd(cwd)
    return select(load_registry(registry_path), cwd, project_binding=project_binding, mode=mode)


def revalidate(selection, *, operation=None, store_name=None):
    if not isinstance(selection, Selection):
        raise ValueError("checked store selection required")
    current = Selection.from_dict(selection.to_dict())
    subsets = {"read": current.readable, "core": current.core, "record": current.recordable,
               "maintenance": current.maintainable}
    if operation is not None:
        if operation not in subsets or store_name is None or not any(store.name == store_name for store in subsets[operation]):
            raise ValueError("selected store does not permit this operation")
    elif store_name is not None:
        current.named(store_name)
    return current


def project_store(service_config, workspace_root, *, database_path, db_id=None,
                  name="project", db_alias="project", purpose="", read=None, record=None, maintenance=None):
    """Normalize the separately authorized project adapter into the common shape."""
    value = {"name": name, "service_config": str(service_config), "db_alias": db_alias,
             "database_path": str(database_path), "db_id": db_id, "purpose": purpose,
             "workspace_root": str(workspace_root), "read": read or {}, "record": record or {},
             "maintenance": maintenance or {}, "when": {"profiles": list(PROFILES)}}
    return _entry(value, None, project=True)


def selection_to_dict(selection):
    return selection.to_dict()


def selection_from_dict(value):
    return Selection.from_dict(value)
