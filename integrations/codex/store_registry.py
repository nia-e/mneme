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
    native_binding: dict | None = field(default=None, repr=False)
    guidance: dict = field(default_factory=dict)

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
        value = {"name": self.name, "service_config": self.service_config,
                "db_alias": self.db_alias, "database_path": self.database_path,
                "db_id": self.db_id, "purpose": self.purpose, "when": asdict(self.when),
                "read": asdict(self.read), "record": asdict(self.record),
                "maintenance": asdict(self.maintenance), "service_digest": self.service_digest,
                "workspace_root": self.workspace_root}
        if self.native_binding is not None:
            value.update(native_binding=self.native_binding, guidance=self.guidance)
        return value

    def service(self):
        """Return the already parsed snapshot only while its file is unchanged."""
        if self.native_binding is not None:
            return NativeService(self)
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
    if isinstance(selection, NativeSelection):
        return selection.revalidate(operation=operation, store_name=store_name)
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
    if isinstance(value, dict) and value.get("schema") == NATIVE_SELECTION_SCHEMA:
        return NativeSelection.from_dict(value)
    return Selection.from_dict(value)


# Native declarative adapter. Python validates the bounded wire contract, never
# parses TOML or repeats native activation/project/profile policy.
NATIVE_SELECTION_SCHEMA = "mneme.native-store-selection.v1"
MAX_NATIVE_SNAPSHOT_BYTES = 512 * 1024


class _NativeBridgeResponse(dict):
    """Keep stdout admission size without adding fields to the native payload."""

    def __init__(self, value, wire_bytes):
        super().__init__(value)
        self.wire_bytes = wire_bytes


def native_bridge(binary, arguments, payload=None, *, timeout=3, limit=MAX_NATIVE_SNAPSHOT_BYTES):
    import subprocess
    import time
    import math
    import host_process
    if (not Path(binary).is_absolute() or type(timeout) not in (int, float)
            or not math.isfinite(timeout) or not 0 < timeout <= 30):
        raise ValueError("invalid bounded native config request")
    raw = json.dumps(payload, ensure_ascii=False, allow_nan=False).encode() if payload is not None else b""
    if len(raw) > 128 * 1024:
        raise ValueError("native config request exceeds byte limit")
    deadline = time.monotonic() + timeout
    process = host_process.spawn([str(binary), "--json", "config", *arguments], tree=True,
                                 stdin=subprocess.PIPE, stdout=subprocess.PIPE, stderr=subprocess.DEVNULL)
    pipe = host_process.BoundedPipe(process, max_read_bytes=min(limit + 1, 16384))
    try:
        pipe.write(raw, deadline)
        process.stdin.close()
        output = bytearray()
        while True:
            chunk = pipe.read(min(16384, limit + 1 - len(output)), deadline)
            if not chunk:
                break
            output.extend(chunk)
            if len(output) > limit:
                raise ValueError("native config response exceeds byte limit")
        process.wait(timeout=max(.001, deadline - time.monotonic()))
        if process.returncode:
            raise ValueError("native configuration refused; reselect before work")
        value = json.loads(output, object_pairs_hook=_unique)
        if not isinstance(value, dict):
            raise ValueError("invalid native configuration response")
        return _NativeBridgeResponse(value, len(output))
    finally:
        pipe.close()
        host_process.terminate_tree(process, timeout=.2)


def native_binary_identity(binary):
    stat = Path(binary).stat()
    return {"device": stat.st_dev, "inode": stat.st_ino, "size": stat.st_size,
            "mtime_ns": stat.st_mtime_ns, "ctime_ns": stat.st_ctime_ns}


def native_select(config_path, binary, cwd):
    config_path, binary, cwd = str(_path(config_path)), str(_path(binary)), str(_path(cwd))
    pin = native_binary_identity(binary)
    snapshot = native_bridge(binary, ["select", "--config", config_path, "--cwd", cwd])
    if native_binary_identity(binary) != pin:
        raise ValueError("native binary changed during selection; use a fresh session")
    return NativeSelection(snapshot, config_path, binary, cwd, pin)


@dataclass(frozen=True)
class NativeSelection:
    snapshot: dict
    registry_path: str
    native_cli: str
    requested_cwd: str | None = None
    binary_pin: dict | None = None

    def __post_init__(self):
        value = self.snapshot
        if (not isinstance(value, dict) or value.get("schema") != "mneme.config-selection.v1"
                or not isinstance(value.get("digest"), str) or not re.fullmatch(r"[0-9a-f]{64}", value["digest"])
                or not isinstance(value.get("active"), list) or len(value["active"]) > 64
                or not isinstance(value.get("config"), dict) or not isinstance(value.get("cwd"), str)
                or not Path(value["cwd"]).is_absolute()):
            raise ValueError("invalid native selection snapshot")
        names, owners = set(), set()
        for row in value["active"]:
            if (not isinstance(row, dict) or not isinstance(row.get("name"), str) or not NAME.fullmatch(row["name"])
                    or not isinstance(row.get("store"), dict) or not isinstance(row.get("eligible"), dict)
                    or set(row["eligible"]) != {"read", "write", "core", "context", "recall", "record", "maintenance"}
                    or any(type(flag) is not bool for flag in row["eligible"].values())):
                raise ValueError("invalid native selected owner")
            store = row["store"]
            if (not isinstance(store.get("id"), str) or not ULID.fullmatch(store["id"])
                    or not isinstance(store.get("owner"), dict) or not NAME.fullmatch(store["owner"].get("db", ""))
                    or row["name"] in names or store["id"] in owners):
                raise ValueError("duplicate or invalid native selected identity")
            names.add(row["name"]); owners.add(store["id"])
        if len(json.dumps(value).encode()) > MAX_NATIVE_SNAPSHOT_BYTES:
            raise ValueError("native selected snapshot exceeds byte limit")

    @property
    def digest(self): return self.snapshot["digest"]
    @property
    def cwd(self): return self.requested_cwd or self.snapshot["cwd"]
    @property
    def canonical_cwd(self): return self.snapshot["cwd"]
    @property
    def project_binding(self): return None
    @property
    def omissions(self): return tuple((row["name"], row.get("reason") or "native activation") for row in self.snapshot.get("stores", []) if not row.get("active"))
    @property
    def stores(self):
        result = []
        for row in self.snapshot["active"]:
            entry, eligible = row["store"], row["eligible"]
            owner, guidance = entry["owner"], entry.get("guidance") or {}
            purpose = "Knowledge and experience belonging to this selected owner."
            capture = entry.get("capture") or {}
            project = self.snapshot.get("project") or {}
            workspace_root = (project.get("root") if project.get("configured") and
                              (entry.get("active") or {}).get("workspace") == "project" else None)
            binding = {"config_path": self.registry_path, "native_cli": self.native_cli,
                       "cwd": self.cwd, "digest": self.digest, "entry": row, "binary_pin": self.binary_pin}
            result.append(SelectedStore(row["name"], self.registry_path, owner["db"], owner.get("path") or "",
                entry["id"], purpose, read=ReadPolicy(eligible["read"], eligible["core"]),
                record=RecordPolicy(eligible["record"], tuple(capture.get("kinds", KINDS)),
                    tuple(capture.get("evidence_kinds", EVIDENCE_KINDS)), tuple(capture.get("required_tags", [])),
                    capture.get("allow_associations", True) and eligible["read"],
                    capture.get("max_summary_bytes", 512), capture.get("max_body_bytes", 2048)),
                maintenance=MaintenancePolicy(eligible["maintenance"], (guidance.get("tag_guide") or {}).get("id")),
                service_digest=self.digest, workspace_root=workspace_root, native_binding=binding, guidance=guidance))
        return tuple(result)
    def subset(self, operation):
        admitted = {row["name"] for row in self.snapshot["active"] if row["eligible"][operation]}
        return tuple(store for store in self.stores if store.name in admitted)
    readable = property(lambda self: self.subset("read"))
    context = property(lambda self: self.subset("context"))
    recallable = property(lambda self: self.subset("recall"))
    core = property(lambda self: self.subset("core"))
    recordable = property(lambda self: self.subset("record"))
    maintainable = property(lambda self: self.subset("maintenance"))
    def named(self, name):
        for store in self.stores:
            if store.name == name: return store
        raise ValueError("store is not selected")
    def to_dict(self):
        return {"schema": NATIVE_SELECTION_SCHEMA, "config_path": self.registry_path,
                "native_cli": self.native_cli, "snapshot": self.snapshot, "digest": self.digest,
                "cwd": self.cwd, "binary_pin": self.binary_pin}
    @classmethod
    def from_dict(cls, value):
        if not isinstance(value, dict) or set(value) != {"schema", "config_path", "native_cli", "snapshot", "digest", "cwd", "binary_pin"}:
            raise ValueError("invalid native frozen selection")
        frozen = cls(value["snapshot"], value["config_path"], value["native_cli"], value["cwd"], value["binary_pin"])
        if frozen.digest != value["digest"] or frozen.cwd != value["cwd"]:
            raise ValueError("native frozen selection differs")
        return frozen.revalidate()
    def revalidate(self, *, operation=None, store_name=None):
        current = native_select(self.registry_path, self.native_cli, self.cwd)
        if current.snapshot != self.snapshot or current.binary_pin != self.binary_pin:
            raise ValueError("native selected configuration changed; use a fresh session")
        if operation is not None:
            operation = "read" if operation == "explicit" else operation
            if operation not in {"read", "write", "core", "context", "recall", "record", "maintenance"} or not any(s.name == store_name for s in current.subset(operation)):
                raise ValueError("selected owner does not permit this operation")
        elif store_name is not None:
            current.named(store_name)
        return current


@dataclass(frozen=True)
class NativeService:
    store: SelectedStore
    token_env = None
    @property
    def database_name(self): return self.store.db_alias
    @property
    def database_path(self): return self.store.database_path
    @property
    def url(self): return "native-config:" + self.store.name
    def native_client(self, timeout=3, *, operation=None):
        if operation is None:
            flags = self.store.native_binding["entry"]["eligible"]
            operation = "record" if flags["record"] else "recall" if flags["recall"] else "core" if flags["core"] else "explicit"
        return NativeClient(self.store, timeout=timeout, operation=operation)


class NativeReadBudgetExhausted(ValueError):
    """A proven read allowance stop, with this call's exact known work charge."""

    def __init__(self, message, *, known_work_bytes):
        super().__init__(message)
        self.known_work_bytes = known_work_bytes


class NativeClient:
    def __init__(self, store, *, timeout=3, operation="explicit", max_total_bytes=None):
        self.store, self.timeout, self.operation = store, timeout, operation
        self.work_bytes = 0
        self._charged_work = 0
        self.max_native_bytes = 512 * 1024
        # Opt-in per-call room for BOTH native receive work and the decoded
        # bridge envelope. The caller owns the aggregate allowance and any
        # final identity-read reserve; existing non-reader callers are unchanged.
        self.max_total_bytes = max_total_bytes
        self.last_read_allowance_bytes = None
    def connect(self): return self
    def close(self): pass
    def __enter__(self): return self.connect()
    def __exit__(self, *_): self.close()
    def call_tool(self, name, arguments):
        self.last_read_allowance_bytes = None
        total_limit = self.max_total_bytes
        if total_limit is not None:
            if type(total_limit) is not int or total_limit < 0:
                raise ValueError("invalid native total byte allowance")
            if total_limit < 2:
                raise NativeReadBudgetExhausted("native total byte allowance exhausted", known_work_bytes=0)
        if type(self.max_native_bytes) is not int or not 0 < self.max_native_bytes <= 16 * 1024 * 1024:
            raise ValueError("native remaining byte allowance exhausted")
        native_limit = self.max_native_bytes
        bridge_options = {}
        if total_limit is not None:
            native_limit = min(native_limit, total_limit // 2)
            bridge_options["limit"] = min(MAX_NATIVE_SNAPSHOT_BYTES, total_limit - native_limit)
        binding = self.store.native_binding
        if binding.get("binary_pin") is not None and native_binary_identity(binding["native_cli"]) != binding["binary_pin"]:
            raise ValueError("native binary changed; use a fresh session")
        response = native_bridge(binding["native_cli"], ["call", "--config", binding["config_path"],
            "--cwd", binding["cwd"], "--operation", self.operation, "--store", self.store.name,
            "--expected-selection-digest", binding["digest"], "--timeout-ms", str(max(1, int(self.timeout * 1000))),
            "--max-native-bytes", str(native_limit), "--canonical-only"],
            {"tool": name, "arguments": arguments}, timeout=self.timeout, **bridge_options)
        if (response.get("schema") != "mneme.config-call.v1" or response.get("selection_digest") != binding["digest"]
                or response.get("store") != self.store.name or type(response.get("ok")) is not bool):
            raise ValueError("native call does not match frozen owner selection")
        work = response.get("work")
        native_bytes = work.get("decoded_bytes") if isinstance(work, dict) else None
        if type(native_bytes) is not int or native_bytes < 0:
            raise ValueError("native call accounting is unknown")
        bridge_bytes = len(json.dumps(response, ensure_ascii=False).encode())
        call_bytes = native_bytes + bridge_bytes
        self.work_bytes += call_bytes
        if not response["ok"]:
            from mcp_client import McpToolError, McpTransportError, McpProtocolError
            error = response.get("error")
            if not isinstance(error, dict) or not isinstance(error.get("message"), str):
                raise McpProtocolError("native config error malformed")
            if total_limit is not None and error.get("kind") == "native" and native_bytes > native_limit:
                # ReceiveBudget charges the refused chunk before returning and
                # before decoding its contents. Strict overshoot proves that
                # refusal; equality or error text cannot distinguish an owner,
                # identity or protocol error. This is NOT a hard wire bound:
                # the refused chunk may consume the caller's final-read reserve.
                raise NativeReadBudgetExhausted("native read byte allowance exhausted", known_work_bytes=call_bytes)
            cls = McpToolError if error.get("kind") == "tool" else McpTransportError if error.get("kind") == "transport" else McpProtocolError
            exception = cls(error["message"][:512])
            exception.details = error
            raise exception
        if "result" not in response:
            raise ValueError("native config result missing")
        result = response["result"]
        # Legacy hook cards have no exact-replica continuation witness. Until
        # that card format supports it, omit rather than flatten cached data
        # into apparent live-owner evidence or guards. Native explicit routes
        # retain the qualified cache reference unchanged.
        if isinstance(result, dict) and "_mneme_source" in result:
            raise ValueError("cached hook result needs exact source-qualified delivery; omitted")
        if total_limit is not None and call_bytes > total_limit:
            raise NativeReadBudgetExhausted("native total byte allowance exhausted", known_work_bytes=call_bytes)
        # Repeating this split admission needs both halves to fit, not merely
        # the last call's sum. This is prospective room for planning a final
        # read, NOT charged work or a guarantee that its response stays this size.
        bridge_wire_bytes = response.wire_bytes if isinstance(response, _NativeBridgeResponse) else bridge_bytes
        self.last_read_allowance_bytes = 2 * max(native_bytes, bridge_bytes, bridge_wire_bytes)
        return result
    def owner_capabilities(self):
        return self.call_tool("owner_capabilities", {})
    def save_verified(self, db, payload, *, expected_db_id=None):
        if db != self.store.db_alias or expected_db_id != self.store.db_id:
            raise ValueError("verified save differs from selected owner")
        return self.call_tool("save/verified", {"db": db, "payload": payload, "expected_db_id": expected_db_id})
    def measured_size(self, value):
        # Full bridge work, including initialize/catalog/framing, is charged
        # exactly once by the caller's existing shared allowance.
        size = self.work_bytes - self._charged_work
        self._charged_work = self.work_bytes
        return size
