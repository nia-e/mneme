#!/usr/bin/env python3
"""Device-local Mneme library registration and opt-in snapshot relay.

This is deliberately separate from the serving MCP process. It never opens a
live Cozo database, and never rewrites a generation that a reader may hold.
"""

from __future__ import annotations

import argparse
import fcntl
import hashlib
import io
import json
import os
import re
from pathlib import Path, PurePosixPath
import shutil
import subprocess
import sys
import tarfile
import time
import uuid
from urllib.parse import urlsplit

MAX_JSON = 1024 * 1024
MAX_FILES = 100_000
MAX_BUNDLE = 512 * 1024 * 1024
MAX_FILE = 512 * 1024 * 1024
SCHEMA = "mneme.library.catalog.v1"
CONFIG_SCHEMA = "mneme.library.config.v1"
PUBLISHER_SCHEMA = "mneme.library.publisher.v1"
MESSAGE_SCHEMA = "mneme.library.message.v1"
ACK_SCHEMA = "mneme.library.ack.v1"
SNAPSHOT_SCHEMA = "mneme.snapshot.v1"


class LibraryError(Exception):
    pass


def _pairs(pairs):
    result = {}
    for key, value in pairs:
        if key in result:
            raise LibraryError(f"duplicate JSON field: {key}")
        result[key] = value
    return result


def _read_json(path, limit=MAX_JSON):
    with Path(path).open("rb") as stream:
        data = stream.read(limit + 1)
    if len(data) > limit:
        raise LibraryError(f"JSON exceeds {limit} bytes")
    try:
        return json.loads(data, object_pairs_hook=_pairs)
    except (ValueError, UnicodeDecodeError) as error:
        raise LibraryError("invalid JSON") from error


def _write_json(path, value):
    path = Path(path)
    encoded = (json.dumps(value, sort_keys=True, separators=(",", ":"),
                          ensure_ascii=False, allow_nan=False) + "\n").encode()
    if len(encoded) > MAX_JSON:
        raise LibraryError("catalog or message exceeds JSON bound")
    path.parent.mkdir(parents=True, exist_ok=True)
    tmp = path.with_name(path.name + ".tmp-" + uuid.uuid4().hex)
    try:
        with tmp.open("xb") as stream:
            os.chmod(tmp, 0o600)
            stream.write(encoded)
            stream.flush()
            os.fsync(stream.fileno())
        os.replace(tmp, path)
        _sync_dir(path.parent)
    finally:
        tmp.unlink(missing_ok=True)


def _sync_dir(path):
    fd = os.open(path, os.O_RDONLY)
    try:
        os.fsync(fd)
    finally:
        os.close(fd)


def _nonempty(value, field):
    if not isinstance(value, str) or not value or len(value.encode()) > 4096 or "\x00" in value:
        raise LibraryError(f"invalid {field}")
    return value


def _path(value, field):
    value = _nonempty(str(value) if isinstance(value, Path) else value, field)
    path = Path(value)
    if not path.is_absolute():
        raise LibraryError(f"{field} must be absolute")
    return path


def _owner_endpoint(value):
    """Enrollment only records this machine's already-running loopback host."""
    if isinstance(value, str):
        value = {"url": value}
    if (not isinstance(value, dict) or "url" not in value
            or set(value) - {"url", "token_env"}):
        raise LibraryError("owner_endpoint must be a loopback URL")
    url = _nonempty(value["url"], "owner endpoint URL")
    try:
        parsed = urlsplit(url)
        port = parsed.port
    except ValueError as error:
        raise LibraryError("invalid owner endpoint URL") from error
    if (parsed.scheme != "http" or parsed.hostname not in ("127.0.0.1", "::1")
            or parsed.username or parsed.password or parsed.query or parsed.fragment
            or port is None or parsed.path not in ("", "/")):
        raise LibraryError("owner endpoint must be numeric loopback HTTP")
    endpoint = {"url": url}
    token_env = value.get("token_env")
    if token_env:
        if not isinstance(token_env, str) or not token_env.isidentifier():
            raise LibraryError("invalid owner token_env")
        endpoint["token_env"] = token_env
    return endpoint


def _set_owner_route(config_path, config, db_id, owner_endpoint):
    if owner_endpoint is None:
        return
    endpoint = _owner_endpoint(owner_endpoint)
    routes = config.setdefault("owner_routes", {})
    if not isinstance(routes, dict):
        raise LibraryError("invalid owner_routes configuration")
    device = routes.setdefault(config["device_id"], {})
    if not isinstance(device, dict):
        raise LibraryError("invalid local owner_routes map")
    old = device.get(db_id)
    if old == endpoint:
        return
    device[db_id] = endpoint
    _write_json(config_path, config)


def _publisher_path(config_path):
    return Path(str(config_path) + ".publisher.json")


def _load_catalog(config_path):
    config_path = Path(config_path).resolve()
    config = _read_json(config_path)
    if not isinstance(config, dict) or config.get("schema") != CONFIG_SCHEMA:
        raise LibraryError("invalid library config schema")
    for name in ("library_id", "device_id", "catalog_path"):
        _nonempty(config.get(name), name)
    catalog_path = Path(config["catalog_path"])
    if not catalog_path.is_absolute():
        catalog_path = config_path.parent / catalog_path
    if catalog_path.exists():
        catalog = _read_json(catalog_path)
        if (not isinstance(catalog, dict) or catalog.get("schema") != SCHEMA
                or catalog.get("library_id") != config["library_id"]
                or not isinstance(catalog.get("revision"), int)
                or isinstance(catalog.get("revision"), bool)
                or not isinstance(catalog.get("entries"), list)):
            raise LibraryError("invalid or foreign library catalog")
    else:
        catalog = {"schema": SCHEMA, "library_id": config["library_id"],
                   "revision": 0, "entries": []}
    return config_path, config, catalog_path, catalog


def _load(config_path):
    config_path, config, catalog_path, catalog = _load_catalog(config_path)
    publisher_path = _publisher_path(config_path)
    publisher = _read_json(publisher_path) if publisher_path.exists() else {
        "schema": PUBLISHER_SCHEMA, "peers": [], "sources": {}, "outbox": [], "awaiting_ack": []}
    if (not isinstance(publisher, dict) or publisher.get("schema") != PUBLISHER_SCHEMA
            or not isinstance(publisher.get("peers"), list)
            or not isinstance(publisher.get("sources"), dict)
            or not isinstance(publisher.get("outbox"), list)
            or not isinstance(publisher.get("awaiting_ack", []), list)):
        raise LibraryError("invalid publisher sidecar")
    publisher.setdefault("awaiting_ack", [])
    return config_path, config, catalog_path, catalog, publisher_path, publisher


def _lock(config_path):
    path = Path(str(config_path) + ".lock")
    path.parent.mkdir(parents=True, exist_ok=True)
    stream = path.open("a+b")
    fcntl.flock(stream, fcntl.LOCK_EX)
    return stream


def _entry(catalog, owner, project):
    return next((e for e in catalog["entries"] if e.get("owner_device_id") == owner
                 and e.get("project_id") == project), None)


def _project_id(device_id, project_root):
    return hashlib.sha256((device_id + "\0" + str(Path(project_root).resolve())).encode()).hexdigest()[:32]


def _component(value, field):
    if not isinstance(value, str) or not re.fullmatch(r"[A-Za-z0-9_-]{1,128}", value):
        raise LibraryError(f"invalid {field}")
    return value


def replica_alias(owner, project, generation):
    owner = _component(owner, "owner device_id")
    project = _component(project, "project_id")
    generation = _component(generation, "generation")
    prefix = hashlib.sha256((owner + "\0" + project).encode()).hexdigest()[:16]
    return f"r_{prefix}_{generation}"


def _replica_path(config_path, owner, project, generation):
    return (Path(config_path).parent / "library-replicas" / _component(owner, "owner device_id")
            / _component(project, "project_id") / _component(generation, "generation")
            / "serve" / "database.db")


def serving_registry(config_path):
    cp, config, catalog_path, catalog, pp, publisher = _load(config_path)
    result = {}
    for entry in catalog["entries"]:
        if entry.get("withdrawn"):
            continue
        for replica in entry.get("replicas", []):
            if replica.get("source_device_id") != config["device_id"]:
                continue
            alias = replica_alias(entry["owner_device_id"], entry["project_id"], replica["generation"])
            path = _replica_path(cp, entry["owner_device_id"], entry["project_id"], replica["generation"])
            if replica.get("database") != alias:
                raise LibraryError("catalog replica alias does not match generation")
            if replica.get("resolved_path") != str(path):
                raise LibraryError("catalog replica path does not match immutable generation")
            validate_bundle(path.parent.parent / "bundle", entry["db_id"])
            if not path.is_file() or path.is_symlink():
                raise LibraryError("serving copy missing")
            if alias in result:
                raise LibraryError("duplicate serving alias")
            result[alias] = path
    return result


def serve(config_path, mcp, http, token_env=None):
    """Exec a dedicated read-only host with generation-pinned DB aliases."""
    mcp = _path(mcp, "mneme-mcp")
    if not isinstance(http, str) or not re.fullmatch(r"(?:127\.0\.0\.1|\[::1\]):[0-9]{1,5}", http):
        raise LibraryError("serve --http must bind numeric loopback")
    registry = serving_registry(config_path)
    if not registry:
        raise LibraryError("no verified replica generations to serve")
    argv = [str(mcp), "--capability-profile", "read-only", "--http", http]
    if token_env is not None:
        if not isinstance(token_env, str) or not token_env.isidentifier():
            raise LibraryError("invalid token environment variable")
        argv += ["--http-token-env", token_env]
    for alias, path in sorted(registry.items()):
        argv += ["--db", f"{alias}={path}"]
    os.execv(str(mcp), argv)


def _eligible(project_root):
    profile = Path(project_root) / ".mneme" / "profile.json"
    if profile.is_symlink():
        raise LibraryError("project profile must not be a symlink")
    if not profile.exists():
        return True
    data = _read_json(profile, 32768)
    if (not isinstance(data, dict) or data.get("schema") != "mneme.profile.v1"
            or set(data) - {"schema", "mode", "library_config"}
            or data.get("mode") not in ("default", "private", "isolated")):
        raise LibraryError("invalid project profile")
    if "library_config" in data:
        _path(data["library_config"], "profile library_config")
    return data["mode"] == "default"


def _queue(publisher, descriptor, bundle=None):
    for peer in publisher["peers"]:
        peer_id = _nonempty(peer.get("device_id"), "peer device_id")
        # Announce only the owner's claim. Replicas learned from other peers
        # are local catalog metadata, never forwarded as multi-hop authority.
        owner_claim = {key: descriptor[key] for key in
                       ("project_id", "db_id", "owner_device_id", "display_name",
                        "database", "revision", "withdrawn")}
        owner_claim["replicas"] = []
        item = {"peer_device_id": peer_id, "descriptor": owner_claim,
                "bundle": str(bundle) if bundle else None,
                "delivery_id": uuid.uuid4().hex}
        # Only the latest owner revision matters to a peer. A failed send stays
        # durable until superseded, and replay is idempotent at the receiver.
        publisher["outbox"] = [old for old in publisher["outbox"] if not (
            old.get("peer_device_id") == peer_id
            and old.get("descriptor", {}).get("project_id") == descriptor["project_id"]
            and old.get("descriptor", {}).get("owner_device_id") == descriptor["owner_device_id"])]
        publisher["awaiting_ack"] = [old for old in publisher["awaiting_ack"] if not (
            old.get("peer_device_id") == peer_id
            and old.get("descriptor", {}).get("project_id") == descriptor["project_id"]
            and old.get("descriptor", {}).get("owner_device_id") == descriptor["owner_device_id"])]
        publisher["outbox"].append(item)


def _record_native_bundle(config_path, project_id, bundle, manifest):
    """Record only a bundle returned by the owner-native snapshot operation."""
    with _lock(config_path):
        cp, config, catalog_path, catalog, pp, publisher = _load(config_path)
        source = publisher["sources"].get(project_id)
        entry = _entry(catalog, config["device_id"], project_id)
        if source is None or entry is None or entry["db_id"] != manifest["db_id"]:
            raise LibraryError("native snapshot no longer matches enrolled source")
        records = source.setdefault("native_bundles", [])
        path = str(Path(bundle).resolve())
        if not any(record.get("path") == path for record in records):
            records.append({"path": path, "db_id": manifest["db_id"],
                            "generation": manifest["generation"]})
        _write_json(pp, publisher)


def _prune_native_bundles(publisher):
    """Remove only exact, recorded, unreferenced old native generations.

    Never scan a shared snapshot root to infer ownership. A failed deletion
    remains recorded for a later retry and visible in status.
    """
    pending = {str(Path(item["bundle"]).resolve()) for item in
               publisher["outbox"] + publisher.get("awaiting_ack", [])
               if item.get("bundle")}
    removed = 0
    for source in publisher["sources"].values():
        records = source.get("native_bundles", [])
        if not isinstance(records, list):
            raise LibraryError("invalid native bundle inventory")
        keep_recent = {r["path"] for r in records[-2:]}
        survivors = []
        for record in records:
            path_text = record.get("path")
            if path_text in keep_recent or path_text in pending:
                survivors.append(record)
                continue
            try:
                path = _path(path_text, "recorded native bundle")
                generation = _component(record.get("generation"), "recorded generation")
                if not path.exists():
                    # The exact recorded path was already removed (for example
                    # a crash after unlink but before sidecar update).
                    continue
                if path.name != generation or path.is_symlink() or not path.is_dir():
                    # Missing or altered inventory is not ours to erase.
                    survivors.append(record)
                    continue
                manifest = validate_bundle(path, record.get("db_id"))
                if manifest["generation"] != generation:
                    survivors.append(record)
                    continue
                shutil.rmtree(path)
                _sync_dir(path.parent)
                removed += 1
            except (LibraryError, OSError):
                survivors.append(record)
        source["native_bundles"] = survivors
    return removed


def _registration_path(path, label):
    """Reject metadata aliases rather than replacing their unrelated targets."""
    path = _path(path, label)
    if path.is_symlink():
        raise LibraryError(f"{label} must not be a symlink")
    if path.parent.is_symlink():
        raise LibraryError(f"{label} directory must not be a symlink")
    if path.exists() and (not path.is_file() or path.stat().st_nlink != 1):
        raise LibraryError(f"{label} must be a singly linked regular file")
    return path


def _validate_registration_config(config):
    """Keep local writes within the existing native runtime-config contract."""
    if set(config) - {"schema", "library_id", "device_id", "catalog_path", "owners",
                      "owner_routes", "replicas", "limits", "core", "rerank"}:
        raise LibraryError("unknown library config field")
    for name in ("owners", "replicas", "owner_routes"):
        if not isinstance(config.get(name, {}), dict):
            raise LibraryError(f"invalid library {name} map")
    routes = config.get("owner_routes", {})
    if len(config.get("owners", {})) > 64 or len(routes) > 64:
        raise LibraryError("library owner routes exceed native bounds")
    endpoint_maps = [config.get("owners", {}), config.get("replicas", {})]
    for device, endpoints in routes.items():
        _nonempty(device, "owner device_id")
        if not isinstance(endpoints, dict) or len(endpoints) > 64:
            raise LibraryError("invalid or oversized library owner routes")
        endpoint_maps.append(endpoints)
    for endpoints in endpoint_maps:
        for identity, endpoint in endpoints.items():
            _nonempty(identity, "endpoint identity")
            if (not isinstance(endpoint, dict) or set(endpoint) - {"url", "token_env", "ssh_mcp_port"}
                    or not isinstance(endpoint.get("url"), str)
                    or (endpoint.get("token_env") is not None and not isinstance(endpoint["token_env"], str))):
                raise LibraryError("invalid library endpoint")
            port = endpoint.get("ssh_mcp_port", 18766)
            if not isinstance(port, int) or isinstance(port, bool) or not 0 <= port <= 65535:
                raise LibraryError("invalid library endpoint port")
    limits = config.get("limits", {})
    if not isinstance(limits, dict) or set(limits) - {"concurrent", "total_candidates", "timeout_ms"}:
        raise LibraryError("invalid library limits")
    for name, maximum in (("concurrent", 4), ("total_candidates", 64), ("timeout_ms", 30000)):
        value = limits.get(name, maximum)
        if not isinstance(value, int) or isinstance(value, bool) or not 1 <= value <= maximum:
            raise LibraryError("invalid library limits")
    if "rerank" in config and not isinstance(config["rerank"], bool):
        raise LibraryError("invalid library rerank flag")
    if config.get("core") is not None:
        core = config["core"]
        if not isinstance(core, dict) or set(core) != {"global_service", "global_database"}:
            raise LibraryError("invalid library core paths")
        for field in core:
            _path(core[field], field)


def register_local_project(config_path, project_root, db_id, *, database, owner_endpoint,
                           display_name=None, create_config=True):
    """Register verified live-owner metadata on this device, never enroll for relay.

    The caller verifies the native owner's db_id, root and registry alias first.
    No live database, publisher sidecar, peer, replica or global-memory store is
    opened or modified here. An explicit withdrawal remains authoritative.
    """
    project_root = _path(project_root, "project root")
    if project_root.is_symlink() or not project_root.is_dir():
        raise LibraryError("project root must be a real directory")
    project_root = project_root.resolve()
    mneme_dir = project_root / ".mneme"
    if mneme_dir.is_symlink() or (mneme_dir.exists() and not mneme_dir.is_dir()):
        raise LibraryError("project .mneme must be a real directory")
    if not _eligible(project_root):
        return False
    db_id = _nonempty(db_id, "verified db_id")
    database = _component(database, "owner database alias")
    endpoint = _owner_endpoint(owner_endpoint)
    display_name = _nonempty(display_name or project_root.name, "display_name")
    if not isinstance(create_config, bool):
        raise LibraryError("create_config must be boolean")
    cp = _registration_path(config_path, "library config")
    cp = cp.resolve()
    _registration_path(Path(str(cp) + ".lock"), "library lock")
    if not cp.exists() and not create_config:
        return False
    with _lock(cp):
        new_config = not cp.exists()
        if new_config:
            # An absent config is not permission to adopt orphaned catalog or
            # publication evidence. Store identity only once, then retry it.
            catalog_path = _registration_path(cp.parent / "catalog.json", "library catalog")
            if catalog_path.exists() or _publisher_path(cp).exists() or _publisher_path(cp).is_symlink():
                raise LibraryError("orphaned library metadata; restore/review the config explicitly")
            config = {"schema": CONFIG_SCHEMA, "library_id": uuid.uuid4().hex,
                      "device_id": uuid.uuid4().hex, "catalog_path": "catalog.json",
                      "owners": {}, "replicas": {}}
            catalog = {"schema": SCHEMA, "library_id": config["library_id"],
                       "revision": 0, "entries": []}
        else:
            # Deliberately do not load publisher state: peer configuration has
            # no authority over ordinary device-local registration.
            if cp.stat().st_size > 64 * 1024:
                raise LibraryError("library config exceeds native 64 KiB bound")
            cp, config, catalog_path, catalog = _load_catalog(cp)
            _registration_path(catalog_path, "library catalog")
            if catalog_path.exists() and catalog_path.stat().st_size > 512 * 1024:
                raise LibraryError("library catalog exceeds native 512 KiB bound")
        _validate_registration_config(config)
        if (set(catalog) - {"schema", "library_id", "revision", "entries"}
                or not isinstance(catalog["revision"], int) or isinstance(catalog["revision"], bool)
                or not 0 <= catalog["revision"] < 2**64 - 1):
            raise LibraryError("invalid library catalog revision")
        ids = set()
        for entry in catalog["entries"]:
            if (not isinstance(entry, dict) or set(entry) - {"project_id", "db_id", "owner_device_id",
                    "display_name", "database", "revision", "withdrawn", "replicas"}):
                raise LibraryError("invalid library project descriptor")
            for field in ("project_id", "db_id", "owner_device_id", "display_name", "database"):
                _nonempty(entry.get(field), field)
            if (entry["project_id"] in ids or not isinstance(entry.get("revision"), int)
                    or isinstance(entry["revision"], bool) or not 0 <= entry["revision"] < 2**64
                    or not isinstance(entry.get("withdrawn", False), bool)
                    or not isinstance(entry.get("replicas", []), list)):
                raise LibraryError("invalid or duplicate library project descriptor")
            ids.add(entry["project_id"])
            replicas = entry.get("replicas", [])
            if len(replicas) > 8:
                raise LibraryError("library descriptor exceeds native replica bound")
            for replica in replicas:
                if (not isinstance(replica, dict) or set(replica) != {"source_device_id", "database",
                        "resolved_path", "generation", "captured_at"}):
                    raise LibraryError("invalid library replica descriptor")
                for field in ("source_device_id", "database", "resolved_path", "generation"):
                    _nonempty(replica[field], field)
                age = replica["captured_at"]
                if not isinstance(age, int) or isinstance(age, bool) or not 0 <= age < 2**64:
                    raise LibraryError("invalid library replica age")
        project = _project_id(config["device_id"], project_root)
        old = _entry(catalog, config["device_id"], project)
        if old and old["db_id"] != db_id:
            raise LibraryError("project identity conflicts with catalog; withdraw/review explicitly")
        if old and old.get("withdrawn"):
            return False
        if old and old["database"] != database:
            raise LibraryError("project owner database alias conflicts with catalog; review explicitly")
        # Validate the route maps before publishing even an empty config.
        routes = config.setdefault("owner_routes", {})
        if not isinstance(routes, dict) or not isinstance(routes.get(config["device_id"], {}), dict):
            raise LibraryError("invalid local owner_routes map")
        device = routes.setdefault(config["device_id"], {})
        changed = device.get(db_id) != endpoint
        device[db_id] = endpoint
        _validate_registration_config(config)
        if len(json.dumps(config, ensure_ascii=False).encode()) > 64 * 1024:
            raise LibraryError("library config exceeds native 64 KiB bound")
        if old is None:
            descriptor = {"project_id": project, "db_id": db_id,
                          "owner_device_id": config["device_id"], "display_name": display_name,
                          "database": database, "revision": 1, "withdrawn": False, "replicas": []}
            catalog["entries"].append(descriptor)
            catalog["revision"] += 1
        if len(catalog["entries"]) > 64 or len(json.dumps(catalog, ensure_ascii=False).encode()) > 512 * 1024:
            raise LibraryError("library catalog exceeds native bounds")
        # Route first: a retry after catalog failure keeps the same identity and
        # creates the missing descriptor, without any relay side effects.
        if new_config or changed:
            _write_json(cp, config)
        if old is None:
            _write_json(catalog_path, catalog)
        return True


def enroll_project(config_path, project_root, db_id=None, *, display_name=None, database=None,
                   owner_endpoint=None, allow_reenroll=False):
    """Idempotent best-effort enrollment after an existing DB identity is known.

    Returns False for an unconfigured/private/isolated project. Never fabricates
    db_id from a filesystem path; a later call can enroll once it is verified.
    """
    config_path = Path(config_path)
    if not config_path.exists() or db_id is None or not _eligible(project_root):
        return False
    if owner_endpoint is not None:
        owner_endpoint = _owner_endpoint(owner_endpoint)
    with _lock(config_path):
        cp, config, catalog_path, catalog, pp, publisher = _load(config_path)
        if not publisher["peers"]:
            return False
        project = _project_id(config["device_id"], project_root)
        old = _entry(catalog, config["device_id"], project)
        if old and old["db_id"] != db_id:
            raise LibraryError("project identity conflicts with catalog; withdraw/review explicitly")
        if old and old.get("withdrawn") and not allow_reenroll:
            # Automatic startup enrollment must not undo an explicit withdrawal.
            return False
        if old and not old.get("withdrawn"):
            if database is not None and old["database"] != _component(database, "owner database alias"):
                raise LibraryError("project owner database alias conflicts with catalog; review explicitly")
            # A locally known descriptor is not yet a reviewed publisher source.
            # Only this explicit enrollment path may promote it to relay state.
            if project not in publisher["sources"]:
                publisher["sources"][project] = {"project_root": str(Path(project_root).resolve()),
                                                 "database": old["database"]}
                _queue(publisher, old)
                _write_json(pp, publisher)
            _set_owner_route(cp, config, db_id, owner_endpoint)
            return True
        # `database` is the owner's MCP registry selector, never a guessed
        # filesystem path. The local project path has no authority to name the
        # running host's database slot.
        database = _component(database or (old["database"] if old else "project"), "owner database alias")
        descriptor = {"project_id": project, "db_id": _nonempty(db_id, "db_id"),
                      "owner_device_id": config["device_id"],
                      "display_name": _nonempty(display_name or Path(project_root).resolve().name, "display_name"),
                      "database": database, "revision": old["revision"] + 1 if old else 1,
                      "withdrawn": False, "replicas": []}
        if old:
            catalog["entries"].remove(old)
        catalog["entries"].append(descriptor)
        catalog["revision"] += 1
        publisher["sources"][project] = {"project_root": str(Path(project_root).resolve()),
                                          "database": database}
        _queue(publisher, descriptor)
        # Outbox first: a catalog crash may leave a resend, never a silent
        # catalog entry with no announcement.
        _write_json(pp, publisher)
        _write_json(catalog_path, catalog)
        _set_owner_route(cp, config, db_id, owner_endpoint)
        return True


def withdraw(config_path, project_id):
    with _lock(config_path):
        cp, config, catalog_path, catalog, pp, publisher = _load(config_path)
        old = _entry(catalog, config["device_id"], project_id)
        if old is None:
            raise LibraryError("unknown owned project")
        if old.get("withdrawn"):
            return old
        old["revision"] += 1
        old["withdrawn"] = True
        old["replicas"] = []
        catalog["revision"] += 1
        # A device-local descriptor has never been announced. Withdrawal is
        # not permission to reveal it to configured peers for the first time.
        if project_id in publisher["sources"]:
            _queue(publisher, old)
            _write_json(pp, publisher)
        _write_json(catalog_path, catalog)
        return old


def _safe_rel(value):
    if not isinstance(value, str) or not value or len(value.encode()) > 4096:
        raise LibraryError("invalid bundle relative path")
    path = PurePosixPath(value)
    if (path.is_absolute() or ".." in path.parts or "." in path.parts
            or value.startswith("/") or "\\" in value or str(path) != value):
        raise LibraryError("unsafe bundle path")
    return value


def validate_bundle(bundle, db_id):
    bundle = Path(bundle)
    if bundle.is_symlink() or not bundle.is_dir():
        raise LibraryError("snapshot bundle is not a real directory")
    manifest = _read_json(bundle / "manifest.json")
    if (not isinstance(manifest, dict) or manifest.get("schema") != SNAPSHOT_SCHEMA
            or manifest.get("db_id") != db_id or not isinstance(manifest.get("generation"), str)
            or not isinstance(manifest.get("captured_at"), int)
            or isinstance(manifest.get("captured_at"), bool)
            or not isinstance(manifest.get("files"), list)
            or not 1 <= len(manifest["files"]) <= MAX_FILES):
        raise LibraryError("invalid or wrong-identity snapshot manifest")
    expected = {"manifest.json"}
    total = 0
    for item in manifest["files"]:
        if not isinstance(item, dict):
            raise LibraryError("invalid manifest file entry")
        name = _safe_rel(item.get("path"))
        if name != "database.db" and not name.startswith("bodies/"):
            raise LibraryError("unexpected bundle path")
        if name in expected:
            raise LibraryError("duplicate bundle path")
        expected.add(name)
        size = item.get("size")
        digest = item.get("sha256")
        if (not isinstance(size, int) or isinstance(size, bool) or not 0 <= size <= MAX_FILE
                or not isinstance(digest, str) or len(digest) != 64
                or any(c not in "0123456789abcdef" for c in digest)):
            raise LibraryError("invalid snapshot size or digest")
        total += size
        if total > MAX_BUNDLE:
            raise LibraryError("snapshot exceeds bundle bound")
        file = bundle / name
        if file.is_symlink() or not file.is_file() or file.stat().st_size != size:
            raise LibraryError("snapshot file absent, symlinked, or wrong size")
        hasher = hashlib.sha256()
        with file.open("rb") as stream:
            while chunk := stream.read(1024 * 1024):
                hasher.update(chunk)
        if hasher.hexdigest() != digest:
            raise LibraryError("snapshot checksum mismatch")
    if "database.db" not in expected:
        raise LibraryError("snapshot database missing")
    actual = set()
    for root, dirs, files in os.walk(bundle, followlinks=False):
        for name in dirs + files:
            path = Path(root) / name
            if path.is_symlink():
                raise LibraryError("snapshot contains symlink")
            if path.is_file():
                actual.add(path.relative_to(bundle).as_posix())
    if actual != expected:
        raise LibraryError("snapshot inventory mismatch")
    return manifest


def _message(descriptor, bundle, delivery_id=None):
    message = {"schema": MESSAGE_SCHEMA, "descriptor": descriptor, "snapshot": None,
               "delivery_id": delivery_id or uuid.uuid4().hex}
    if bundle:
        manifest = validate_bundle(bundle, descriptor["db_id"])
        message["snapshot"] = manifest
    return message


def _make_archive(descriptor, bundle, delivery_id=None):
    message = _message(descriptor, bundle, delivery_id)
    output = io.BytesIO()
    with tarfile.open(fileobj=output, mode="w") as archive:
        payload = json.dumps(message, sort_keys=True, separators=(",", ":")).encode()
        info = tarfile.TarInfo("message.json")
        info.size = len(payload)
        archive.addfile(info, io.BytesIO(payload))
        if bundle:
            manifest = message["snapshot"]
            names = ["manifest.json"] + [x["path"] for x in manifest["files"]]
            for name in names:
                archive.add(Path(bundle) / name, arcname="snapshot/" + name, recursive=False)
    data = output.getvalue()
    if len(data) > MAX_BUNDLE + MAX_JSON + 1024 * 1024:
        raise LibraryError("archive exceeds bound")
    return data


def _peer_send(peer, data, delivery_id):
    transport = peer.get("transport")
    if transport == "local_directory":
        inbox = _path(peer.get("inbox"), "peer inbox")
        inbox.mkdir(parents=True, exist_ok=True)
        destination = inbox / (delivery_id + ".tar")
        if destination.exists():
            if destination.is_symlink() or destination.read_bytes() != data:
                raise LibraryError("local delivery identity conflict")
            return None
        tmp = inbox / (delivery_id + ".part-" + uuid.uuid4().hex)
        try:
            with tmp.open("xb") as stream:
                os.chmod(tmp, 0o600)
                stream.write(data)
                stream.flush()
                os.fsync(stream.fileno())
            os.replace(tmp, destination)
        finally:
            tmp.unlink(missing_ok=True)
        _sync_dir(inbox)
        return None
    elif transport == "ssh":
        host = _nonempty(peer.get("host"), "SSH host")
        if host.startswith("-") or any(c.isspace() for c in host) or len(host) > 255:
            raise LibraryError("invalid SSH host")
        port = peer.get("port", 22)
        if not isinstance(port, int) or isinstance(port, bool) or not 1 <= port <= 65535:
            raise LibraryError("invalid SSH port")
        # Receiver command is deliberately fixed. Configure its trusted peer
        # identity and config path on the remote host, not from this payload.
        proc = subprocess.run(["ssh", "-T", "-oBatchMode=yes", "-oStrictHostKeyChecking=yes",
                               "-p", str(port), "--", host,
                               "exec ~/.local/bin/mneme-library-receive"],
                              input=data, stdout=subprocess.PIPE,
                              stderr=subprocess.DEVNULL, timeout=120, check=False)
        if proc.returncode:
            raise LibraryError("SSH library delivery failed; announcement remains pending")
        if len(proc.stdout) > MAX_JSON:
            raise LibraryError("SSH library acknowledgement exceeds bound")
        try:
            return json.loads(proc.stdout, object_pairs_hook=_pairs)
        except (ValueError, UnicodeDecodeError) as error:
            raise LibraryError("SSH library acknowledgement is invalid") from error
    else:
        raise LibraryError("unknown peer transport")


def _merge_ack(config, catalog, item, response):
    if not isinstance(response, dict) or response.get("result") not in ("announced", "activated", "replay"):
        raise LibraryError("peer did not acknowledge delivery")
    ack = response.get("ack")
    descriptor = item["descriptor"]
    if (not isinstance(ack, dict) or ack.get("schema") != ACK_SCHEMA
            or ack.get("delivery_id") != item["delivery_id"]
            or ack.get("receiver_device_id") != item["peer_device_id"]
            or any(ack.get(key) != descriptor.get(key) for key in
                   ("owner_device_id", "project_id", "db_id", "revision"))):
        raise LibraryError("peer acknowledgement identity mismatch")
    entry = _entry(catalog, descriptor["owner_device_id"], descriptor["project_id"])
    if (entry is None or entry["db_id"] != descriptor["db_id"]
            or entry["revision"] != descriptor["revision"]):
        # An old acknowledgement cannot roll back a newer owner revision.
        return
    replicas = ack.get("replicas")
    if not isinstance(replicas, list) or len(replicas) > 2:
        raise LibraryError("invalid peer replica acknowledgement")
    validated = []
    for replica in replicas:
        if not isinstance(replica, dict) or replica.get("source_device_id") != item["peer_device_id"]:
            raise LibraryError("peer replica source mismatch")
        generation = _component(replica.get("generation"), "ack generation")
        if replica.get("database") != replica_alias(descriptor["owner_device_id"],
                                                     descriptor["project_id"], generation):
            raise LibraryError("peer replica alias mismatch")
        _path(replica.get("resolved_path"), "peer resolved_path")
        captured = replica.get("captured_at")
        if not isinstance(captured, int) or isinstance(captured, bool) or captured < 0:
            raise LibraryError("invalid peer snapshot time")
        validated.append({k: replica[k] for k in ("source_device_id", "database", "resolved_path",
                                                   "generation", "captured_at")})
    previous = [r for r in entry.get("replicas", [])
                if r.get("source_device_id") == item["peer_device_id"]]
    if previous and (not validated or
                     (validated[0]["captured_at"], validated[0]["generation"])
                     < (previous[0]["captured_at"], previous[0]["generation"])):
        return
    if previous == validated:
        return
    other = [r for r in entry.get("replicas", [])
             if r.get("source_device_id") != item["peer_device_id"]]
    entry["replicas"] = other + validated
    catalog["revision"] += 1


def sync(config_path):
    # A project may become private after enrollment but before an offline
    # announcement is delivered. Supersede it with a withdrawal first.
    cp, config, catalog_path, catalog, pp, publisher = _load(config_path)
    for entry in list(catalog["entries"]):
        source = publisher["sources"].get(entry.get("project_id"), {})
        if (entry.get("owner_device_id") == config["device_id"]
                and not entry.get("withdrawn") and source.get("project_root")
                and not _eligible(source["project_root"])):
            withdraw(config_path, entry["project_id"])
    with _lock(config_path):
        cp, config, catalog_path, catalog, pp, publisher = _load(config_path)
        peers = {p.get("device_id"): p for p in publisher["peers"]}
        for item in publisher["outbox"]:
            item.setdefault("delivery_id", uuid.uuid4().hex)
        _write_json(pp, publisher)
        accepted_ack_files = []
        still_awaiting = []
        for item in publisher["awaiting_ack"]:
            peer = peers.get(item.get("peer_device_id"))
            if not peer or peer.get("transport") != "local_directory":
                still_awaiting.append(item)
                continue
            ack_file = _path(peer.get("inbox"), "peer inbox") / (item["delivery_id"] + ".ack.json")
            if not ack_file.is_file() or ack_file.is_symlink():
                still_awaiting.append(item)
                continue
            try:
                _merge_ack(config, catalog, item, _read_json(ack_file))
                accepted_ack_files.append(ack_file)
            except (LibraryError, OSError):
                still_awaiting.append(item)
        publisher["awaiting_ack"] = still_awaiting
        pending = []
        delivered = 0
        for item in publisher["outbox"]:
            try:
                peer = peers[item["peer_device_id"]]
                response = _peer_send(peer, _make_archive(item["descriptor"], item.get("bundle"),
                                                         item["delivery_id"]), item["delivery_id"])
                if peer.get("transport") == "local_directory":
                    publisher["awaiting_ack"].append(item)
                else:
                    _merge_ack(config, catalog, item, response)
                delivered += 1
            except (OSError, LibraryError, subprocess.TimeoutExpired, KeyError):
                pending.append(item)
        publisher["outbox"] = pending
        # Durable delivery acknowledgement must precede any source cleanup:
        # on crash, an old outbox must never point at deleted bundle bytes.
        _write_json(catalog_path, catalog)
        _write_json(pp, publisher)
        for ack_file in accepted_ack_files:
            ack_file.unlink(missing_ok=True)
        removed = _prune_native_bundles(publisher)
        _write_json(pp, publisher)
        return {"delivered": delivered,
                "pending": len(pending) + len(publisher["awaiting_ack"]),
                "source_bundles_pruned": removed}


def _extract(data, stage):
    if len(data) > MAX_BUNDLE + MAX_JSON + 1024 * 1024:
        raise LibraryError("message exceeds bound")
    with tarfile.open(fileobj=io.BytesIO(data), mode="r:") as archive:
        members = archive.getmembers()
        if not members or len(members) > MAX_FILES + 2:
            raise LibraryError("invalid archive inventory")
        seen = set()
        for member in members:
            name = _safe_rel(member.name)
            if name in seen or not member.isfile() or member.size > MAX_FILE:
                raise LibraryError("unsafe archive member")
            seen.add(name)
            if name != "message.json" and not name.startswith("snapshot/"):
                raise LibraryError("unexpected archive member")
            dest = stage / name
            dest.parent.mkdir(parents=True, exist_ok=True)
            with archive.extractfile(member) as source, dest.open("xb") as target:
                shutil.copyfileobj(source, target, 1024 * 1024)
                target.flush()
                os.fsync(target.fileno())
        if "message.json" not in seen:
            raise LibraryError("missing message")
    message = _read_json(stage / "message.json")
    if not isinstance(message, dict) or message.get("schema") != MESSAGE_SCHEMA:
        raise LibraryError("invalid message schema")
    return message


def _service_command(publisher, action):
    serving = publisher.get("serving")
    if serving is None:
        return
    if not isinstance(serving, dict):
        raise LibraryError("invalid serving config")
    argv = serving.get(action)
    if (not isinstance(argv, list) or not argv or len(argv) > 16
            or any(not isinstance(x, str) or not x or "\x00" in x or len(x) > 4096
                   for x in argv)):
        raise LibraryError(f"invalid serving {action} argv")
    _path(argv[0], "serving executable")
    result = subprocess.run(argv, stdout=subprocess.DEVNULL,
                            stderr=subprocess.DEVNULL, timeout=30, check=False)
    if result.returncode:
        raise LibraryError(f"serving {action} failed; catalog not switched" if action == "stop"
                           else "serving restart failed; verified generation is selected")


def _receive_result(result, config, catalog, message, owner, project, include_ack):
    if not include_ack:
        return result
    descriptor = message["descriptor"]
    local = _entry(catalog, owner, project)
    replicas = [] if local is None else [r for r in local.get("replicas", [])
                                         if r.get("source_device_id") == config["device_id"]]
    return {"result": result, "ack": {"schema": ACK_SCHEMA,
            "delivery_id": message["delivery_id"],
            "receiver_device_id": config["device_id"], "owner_device_id": owner,
            "project_id": project, "db_id": descriptor["db_id"],
            "revision": descriptor["revision"], "replicas": replicas}}


def receive(config_path, data, *, authenticated_peer, include_ack=False):
    """Validate privately, then activate only a coherent generation.

    authenticated_peer is supplied by a trusted local inbox or a fixed SSH
    receiver wrapper, never by the incoming message itself.
    """
    with _lock(config_path):
        cp, config, catalog_path, catalog, pp, publisher = _load(config_path)
        peer = next((p for p in publisher["peers"] if p.get("device_id") == authenticated_peer), None)
        if peer is None:
            raise LibraryError("untrusted peer")
        state = cp.parent / "library-replicas"
        state.mkdir(parents=True, exist_ok=True)
        stage = state / (".stage-" + uuid.uuid4().hex)
        stage.mkdir(mode=0o700)
        try:
            message = _extract(data, stage)
            delivery_id = message.get("delivery_id")
            if not isinstance(delivery_id, str) or not re.fullmatch(r"[0-9a-f]{32}", delivery_id):
                raise LibraryError("invalid delivery identity")
            descriptor = message.get("descriptor")
            if not isinstance(descriptor, dict):
                raise LibraryError("missing descriptor")
            owner = _component(descriptor.get("owner_device_id"), "owner_device_id")
            project = _component(descriptor.get("project_id"), "project_id")
            db_id = _nonempty(descriptor.get("db_id"), "db_id")
            _nonempty(descriptor.get("display_name"), "display_name")
            _component(descriptor.get("database"), "owner database alias")
            if owner != authenticated_peer:
                raise LibraryError("announcement owner does not match authenticated peer")
            revision = descriptor.get("revision")
            if not isinstance(revision, int) or isinstance(revision, bool) or revision < 1:
                raise LibraryError("invalid owner revision")
            old = _entry(catalog, owner, project)
            if old and old["db_id"] != db_id:
                raise LibraryError("owner project identity conflict")
            if old and revision < old["revision"]:
                return _receive_result("stale", config, catalog, message, owner, project, include_ack)
            if old and revision == old["revision"]:
                # A same-revision snapshot may fill a previously pending
                # descriptor, but contradictory metadata is never LWW.
                for key in ("display_name", "database", "withdrawn"):
                    if descriptor.get(key) != old.get(key):
                        raise LibraryError("same-revision descriptor conflict")
            snapshot = message.get("snapshot")
            withdrawn = descriptor.get("withdrawn")
            if not isinstance(withdrawn, bool):
                raise LibraryError("invalid withdrawal flag")
            if withdrawn and snapshot is not None:
                raise LibraryError("withdrawal cannot carry snapshot")
            replicas = old.get("replicas", []) if old else []
            known_old_generations = {r.get("generation") for r in replicas}
            if snapshot is not None:
                manifest = validate_bundle(stage / "snapshot", db_id)
                if manifest != snapshot:
                    raise LibraryError("message and bundle manifest disagree")
                new_order = (manifest["captured_at"], manifest["generation"])
                current_order = ((replicas[0]["captured_at"], replicas[0]["generation"])
                                 if replicas else None)
                if current_order is not None and new_order < current_order:
                    return _receive_result("stale", config, catalog, message, owner, project, include_ack)
                generation = _component(manifest["generation"], "generation")
                target_parent = state / owner / project
                target_parent.mkdir(parents=True, exist_ok=True)
                # On the first replica, durability of the nested parent names
                # is separate from durability of the generation rename.
                _sync_dir(cp.parent)
                _sync_dir(state)
                _sync_dir(state / owner)
                _sync_dir(target_parent)
                target = target_parent / generation
                if target.exists():
                    if validate_bundle(target / "bundle", db_id) != manifest:
                        raise LibraryError("same-generation snapshot conflict")
                else:
                    # Native bundle has `bodies/`; Mneme's open path for
                    # `database.db` expects the sibling `database.bodies/`.
                    # Keep the original bundle separately for retry proof.
                    incoming = stage / "snapshot"
                    prepared = stage / "prepared"
                    prepared.mkdir()
                    os.rename(incoming, prepared / "bundle")
                    serving = prepared / "serve"
                    serving.mkdir()
                    shutil.copy2(prepared / "bundle" / "database.db", serving / "database.db")
                    bodies = prepared / "bundle" / "bodies"
                    if bodies.exists():
                        shutil.copytree(bodies, serving / "database.bodies")
                    for root, dirs, files in os.walk(prepared):
                        for name in files:
                            with (Path(root) / name).open("rb") as stream:
                                os.fsync(stream.fileno())
                        _sync_dir(root)
                    os.rename(prepared, target)
                    _sync_dir(target_parent)
                replica = {"source_device_id": config["device_id"],
                           "database": replica_alias(owner, project, generation),
                           "resolved_path": str(target / "serve" / "database.db"),
                           "generation": generation,
                           "captured_at": manifest["captured_at"]}
                replicas = [replica] + [r for r in replicas if r.get("generation") != generation]
                replicas = replicas[:2]
            if old and revision == old["revision"] and snapshot is None:
                return _receive_result("replay", config, catalog, message, owner, project, include_ack)
            new = {k: descriptor[k] for k in ("project_id", "db_id", "owner_device_id",
                                                 "display_name", "database", "revision", "withdrawn")}
            new["replicas"] = [] if withdrawn else replicas
            if old:
                catalog["entries"].remove(old)
            catalog["entries"].append(new)
            catalog["revision"] += 1
            if snapshot is not None:
                # A dedicated replica host holds its own serving copy. Stop it
                # before changing the catalog selector, then start it against
                # the new immutable generation; never replace an open file.
                _service_command(publisher, "stop")
            _write_json(catalog_path, catalog)
            if snapshot is not None:
                _service_command(publisher, "start")
            # Prune only generations we created and no longer advertise.
            if snapshot is not None:
                keep = {r["generation"] for r in replicas}
                for generation in known_old_generations - keep:
                    # Only a generation previously advertised by our own
                    # catalog is recognized cleanup inventory. Unknown residue
                    # remains for operator review.
                    child = target_parent / _component(generation, "known generation")
                    if child.is_dir() and not child.is_symlink():
                        shutil.rmtree(child)
            return _receive_result("activated" if snapshot is not None else "announced",
                                   config, catalog, message, owner, project, include_ack)
        finally:
            shutil.rmtree(stage, ignore_errors=True)


def publish(config_path, project_id, bundle):
    with _lock(config_path):
        cp, config, catalog_path, catalog, pp, publisher = _load(config_path)
        descriptor = _entry(catalog, config["device_id"], project_id)
        if not descriptor or descriptor.get("withdrawn") or project_id not in publisher["sources"]:
            raise LibraryError("project not enrolled or withdrawn")
        source = publisher["sources"].get(project_id, {})
        if source.get("project_root") and not _eligible(source["project_root"]):
            raise LibraryError("private or isolated project cannot publish")
        validate_bundle(bundle, descriptor["db_id"])
        _queue(publisher, descriptor, Path(bundle).resolve())
        _write_json(pp, publisher)
    return sync(config_path)


def _snapshot(config_path, project_id):
    cp, config, catalog_path, catalog, pp, publisher = _load(config_path)
    descriptor = _entry(catalog, config["device_id"], project_id)
    if not descriptor or descriptor.get("withdrawn") or project_id not in publisher["sources"]:
        raise LibraryError("project not enrolled or withdrawn")
    source = publisher["sources"].get(project_id, {})
    if source.get("project_root") and not _eligible(source["project_root"]):
        raise LibraryError("private or isolated project cannot snapshot for library")
    settings = publisher.get("snapshot", {})
    if not isinstance(settings, dict):
        raise LibraryError("invalid snapshot settings")
    binary = _path(settings.get("mnemed"), "mnemed")
    exact = config.get("owner_routes", {}).get(config["device_id"], {}).get(descriptor["db_id"])
    legacy = config.get("owners", {}).get(config["device_id"])
    endpoint = exact or legacy
    url = _nonempty(endpoint.get("url") if isinstance(endpoint, dict)
                    else settings.get("remote_url"), "snapshot remote_url")
    parsed = urlsplit(url)
    if (parsed.scheme != "http" or parsed.hostname not in ("127.0.0.1", "::1")
            or parsed.username or parsed.password or parsed.query or parsed.fragment
            or parsed.port is None):
        raise LibraryError("snapshot endpoint must be numeric loopback HTTP")
    argv = [str(binary), "--json", "--remote", url, "--remote-db", descriptor["database"],
            "snapshot", "create"]
    token_env = endpoint.get("token_env") if isinstance(endpoint, dict) else settings.get("token_env")
    if token_env:
        if not isinstance(token_env, str) or not token_env.isidentifier():
            raise LibraryError("invalid snapshot token_env")
        argv[4:4] = ["--remote-token-env", token_env]
    try:
        result = subprocess.run(argv, capture_output=True, timeout=120, check=False)
    except subprocess.TimeoutExpired as error:
        raise LibraryError("snapshot timed out; no publication queued") from error
    if result.returncode:
        # Busy refusal is best-effort. Never echo server stderr or tokens.
        raise LibraryError("snapshot unavailable or busy; no publication queued")
    if len(result.stdout) > MAX_JSON:
        raise LibraryError("snapshot response exceeds bound")
    try:
        value = json.loads(result.stdout)
    except ValueError as error:
        raise LibraryError("invalid snapshot response") from error
    if (not isinstance(value, dict) or value.get("db_id") != descriptor["db_id"]
            or not isinstance(value.get("bundle"), str)):
        raise LibraryError("snapshot response identity mismatch")
    bundle = _path(value["bundle"], "snapshot bundle")
    manifest = validate_bundle(bundle, descriptor["db_id"])
    if manifest["generation"] != value.get("generation"):
        raise LibraryError("snapshot generation mismatch")
    _record_native_bundle(config_path, project_id, bundle, manifest)
    return bundle


def once(config_path):
    cp, config, catalog_path, catalog, pp, publisher = _load(config_path)
    now = int(time.time())
    due = []
    for entry in catalog["entries"]:
        if (entry.get("owner_device_id") == config["device_id"]
                and not entry.get("withdrawn")
                and entry["project_id"] in publisher["sources"]
                and now - publisher["sources"].get(entry["project_id"], {}).get("last_snapshot_at", 0) >= 900):
            due.append(entry["project_id"])
    results = {}
    for project in due:
        try:
            bundle = _snapshot(config_path, project)
            results[project] = publish(config_path, project, bundle)
            with _lock(config_path):
                cp, config, catalog_path, catalog, pp, publisher = _load(config_path)
                publisher["sources"][project]["last_snapshot_at"] = int(time.time())
                _write_json(pp, publisher)
        except (LibraryError, OSError) as error:
            results[project] = {"skipped": str(error)}
    results["delivery"] = sync(config_path)
    results["receive"] = receive_inbox(config_path)
    return results


def receive_inbox(config_path):
    cp, config, catalog_path, catalog, pp, publisher = _load(config_path)
    accepted = rejected = 0
    for peer in publisher["peers"]:
        inbox_name = peer.get("receive_inbox")
        if inbox_name is None:
            continue
        inbox = _path(inbox_name, "receive inbox")
        if not inbox.exists():
            continue
        for file in sorted(inbox.glob("*.tar"))[:100]:
            try:
                if file.is_symlink() or file.stat().st_size > MAX_BUNDLE + MAX_JSON + 1024 * 1024:
                    raise LibraryError("unsafe inbox archive")
                response = receive(config_path, file.read_bytes(),
                                   authenticated_peer=peer["device_id"], include_ack=True)
                if response["ack"]["delivery_id"] != file.stem:
                    raise LibraryError("inbox filename and delivery identity differ")
                _write_json(file.with_suffix(".ack.json"), response)
                file.unlink()
                accepted += 1
            except (LibraryError, OSError):
                # Leave evidence for operator review, not an infinite automatic
                # delete/retry that could hide a hostile or partial delivery.
                rejected += 1
    return {"accepted": accepted, "rejected": rejected}


def status(config_path):
    cp, config, catalog_path, catalog, pp, publisher = _load(config_path)
    now = int(time.time())
    pending = {(x["descriptor"]["owner_device_id"], x["descriptor"]["project_id"])
               for x in publisher["outbox"] + publisher["awaiting_ack"]}
    return {"library_id": config["library_id"], "device_id": config["device_id"],
            "pending": len(publisher["outbox"]) + len(publisher["awaiting_ack"]),
            "source_cleanup_pending": sum(max(0, len(s.get("native_bundles", [])) - 2)
                                          for s in publisher["sources"].values()),
            "entries": [
                dict(entry, delivery=("pending" if (entry["owner_device_id"], entry["project_id"]) in pending
                                      else "local" if entry["owner_device_id"] == config["device_id"]
                                      and entry["project_id"] not in publisher["sources"] else "sent"),
                     replica_age_seconds=(now - entry["replicas"][0]["captured_at"])
                     if entry.get("replicas") else None)
                for entry in catalog["entries"]]}


def main(argv=None):
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--config", required=True, type=Path)
    commands = parser.add_subparsers(dest="command", required=True)
    enroll = commands.add_parser("enroll")
    enroll.add_argument("--project-root", required=True, type=Path)
    enroll.add_argument("--db-id", required=True)
    enroll.add_argument("--display-name")
    enroll.add_argument("--database")
    enroll.add_argument("--owner-endpoint", help="local owner MCP loopback URL; stored only in local config")
    enroll.add_argument("--reenroll", action="store_true",
                        help="explicitly restore a previously withdrawn project")
    withdraw_cmd = commands.add_parser("withdraw")
    withdraw_cmd.add_argument("--project-id", required=True)
    commands.add_parser("status")
    pub = commands.add_parser("publish")
    pub.add_argument("--project-id", required=True)
    pub.add_argument("--bundle", type=Path)
    rec = commands.add_parser("receive")
    rec.add_argument("--authenticated-peer", required=True)
    rec.add_argument("--input", type=Path)
    commands.add_parser("sync")
    commands.add_parser("once")
    serving = commands.add_parser("serve", help="exec a generation-pinned read-only replica MCP host")
    serving.add_argument("--mcp", required=True)
    serving.add_argument("--http", required=True)
    serving.add_argument("--http-token-env")
    args = parser.parse_args(argv)
    try:
        if args.command == "enroll":
            result = {"enrolled": enroll_project(args.config, args.project_root, args.db_id,
                                                  display_name=args.display_name,
                                                  database=args.database,
                                                  owner_endpoint=args.owner_endpoint,
                                                  allow_reenroll=args.reenroll)}
        elif args.command == "withdraw":
            result = withdraw(args.config, args.project_id)
        elif args.command == "status":
            result = status(args.config)
        elif args.command == "publish":
            result = publish(args.config, args.project_id,
                             args.bundle or _snapshot(args.config, args.project_id))
        elif args.command == "receive":
            data = args.input.read_bytes() if args.input else sys.stdin.buffer.read(MAX_BUNDLE + MAX_JSON + 1024 * 1024 + 1)
            result = receive(args.config, data, authenticated_peer=args.authenticated_peer,
                             include_ack=True)
        elif args.command == "once":
            result = once(args.config)
        elif args.command == "serve":
            serve(args.config, args.mcp, args.http, args.http_token_env)
            raise AssertionError("exec returned")
        else:
            result = sync(args.config)
        print(json.dumps(result, sort_keys=True))
        return 0
    except (LibraryError, OSError, subprocess.TimeoutExpired) as error:
        print(f"library {args.command}: {error}", file=sys.stderr)
        return 1


if __name__ == "__main__":
    raise SystemExit(main())
