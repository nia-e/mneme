#!/usr/bin/env python3
"""Bounded demand-only cache population and a fixed authenticated SSH exporter.

Neither entry point opens a live database, follows body links, imports routes, or
enumerates owners. Native exact-selection replay authorizes each population;
locally pinned peers/export maps authorize transport and snapshot capture.
"""
from __future__ import annotations

import argparse
from contextlib import contextmanager
try:
    import fcntl
except ImportError:  # Native snapshots are intentionally unsupported on Windows.
    fcntl = None
import hashlib
import json
import os
from pathlib import Path
import re
import selectors
import shutil
import stat
import subprocess
import sys
import time
import uuid

import library
import host_process  # library installs the source-tree helper path when needed

Error = library.LibraryError
MIB = 1024 * 1024
MAX_RECEIPTS = 128
MAX_SCAN_FILES = 500_000
MAX_DEMAND = 64 * 1024
ARCHIVE_OVERHEAD = 2 * MIB
EXPORT_SCHEMA = "mneme.cache-export-request.v1"


def _closed(value, fields, label, required=()):
    if not isinstance(value, dict) or set(value) - set(fields) or set(required) - set(value):
        raise Error(f"invalid {label} fields")
    return value


def _integer(value, low, high, label):
    if type(value) is not int or not low <= value <= high:
        raise Error(f"invalid {label}")
    return value


def _deadline(deadline):
    if time.monotonic() >= deadline:
        raise Error("cache operation deadline exceeded")


def _private_json(path, limit=library.MAX_JSON):
    path = library._path(path, "private cache file")
    metadata = path.lstat()
    if (not stat.S_ISREG(metadata.st_mode) or metadata.st_nlink != 1
            or metadata.st_uid != os.geteuid() or stat.S_IMODE(metadata.st_mode) != 0o600):
        raise Error("cache file must be owner-only mode 0600 and singly linked")
    return library._read_json(path, limit)


def _private_directory(path):
    descriptor = library._open_directory(path)
    try:
        metadata = os.fstat(descriptor)
        if metadata.st_uid != os.geteuid() or stat.S_IMODE(metadata.st_mode) != 0o700:
            raise Error("cache directory must be current-owner mode 0700")
    finally:
        os.close(descriptor)


@contextmanager
def _single_worker(path):
    # Never queue a second background capture behind a slow first one.
    with library._open_file(Path(str(path) + ".cache.lock"), "a+b") as stream:
        try:
            fcntl.flock(stream.fileno(), fcntl.LOCK_EX | fcntl.LOCK_NB)
        except BlockingIOError as error:
            raise Error("cache worker already active") from error
        try:
            yield
        finally:
            fcntl.flock(stream.fileno(), fcntl.LOCK_UN)


def _run(argv, payload, bound, deadline):
    """Drain bounded output incrementally; no communicate()-then-check fuse."""
    _deadline(deadline)
    process = host_process.spawn(argv, tree=True, stdin=subprocess.PIPE,
                                 stdout=subprocess.PIPE, stderr=subprocess.DEVNULL)
    pipe = host_process.BoundedPipe(process, max_read_bytes=64 * 1024)
    output = bytearray()
    try:
        pipe.write(payload, deadline)
        process.stdin.close()
        while True:
            chunk = pipe.read(min(64 * 1024, bound + 1 - len(output)), deadline)
            if not chunk:
                break
            output.extend(chunk)
            if len(output) > bound:
                raise Error("cache subprocess output exceeds bound")
        _deadline(deadline)
        if process.wait(timeout=max(0.001, deadline - time.monotonic())):
            raise Error("cache subprocess unavailable, busy, or refused; no retry this run")
        return bytes(output)
    finally:
        pipe.close()
        host_process.terminate_tree(process, timeout=0.1)


def _json_output(raw):
    try:
        return json.loads(raw, object_pairs_hook=library._pairs)
    except (ValueError, UnicodeDecodeError) as error:
        raise Error("invalid native cache plan JSON") from error


def _usage(roots, ceiling, deadline):
    """Count all residue too; unknown state is not free space or GC authority."""
    total = count = 0
    seen = set()
    for root in roots:
        root = Path(root)
        if root in seen:
            continue
        seen.add(root)
        if not root.exists():
            continue
        if root.is_symlink():
            raise Error("cache accounting root must not be a symlink")
        for base, dirs, files in os.walk(root, followlinks=False):
            _deadline(deadline)
            for name in dirs + files:
                count += 1
                if count > MAX_SCAN_FILES:
                    raise Error("cache accounting inventory exceeds bound")
                path = Path(base) / name
                metadata = path.lstat()
                if stat.S_ISLNK(metadata.st_mode):
                    raise Error("cache accounting inventory contains a symlink")
                if stat.S_ISREG(metadata.st_mode):
                    if metadata.st_nlink != 1:
                        raise Error("cache accounting inventory contains a hard link")
                    total += metadata.st_size
                    if total > ceiling:
                        raise Error("cache disk ceiling exceeded; old generations retained")
                elif not stat.S_ISDIR(metadata.st_mode):
                    raise Error("cache accounting inventory contains a special file")
    return total


def _reserve(roots, ceiling, required, deadline):
    used = _usage(roots, ceiling, deadline)
    if used + required > ceiling:
        raise Error("insufficient configured cache capacity; old generations retained")
    for root in roots:
        ancestor = Path(root)
        while not ancestor.exists():
            ancestor = ancestor.parent
        if shutil.disk_usage(ancestor).free < required:
            raise Error("insufficient filesystem space for bounded cache preparation")


def _common_config(path, schema, fields):
    path = library._path(path, "cache config")
    _private_directory(path.parent)
    config = _private_json(path)
    _closed(config, fields, "cache config", ("schema",))
    if config["schema"] != schema:
        raise Error("wrong cache config schema")
    config["max_run_secs"] = _integer(config.get("max_run_secs", 120), 1, 120, "max_run_secs")
    config["max_disk_bytes"] = _integer(config.get("max_disk_bytes", 2 * 1024 * MIB),
                                        MIB, 1024 * 1024 * MIB, "max_disk_bytes")
    return config


def _export_config(path):
    config = _common_config(path, "mneme.cache-exporter.v1",
            ("schema", "device_id", "stores", "max_run_secs", "max_disk_bytes", "max_snapshot_bytes"))
    library._component(config.get("device_id"), "device_id")
    config["max_snapshot_bytes"] = _integer(config.get("max_snapshot_bytes", library.MAX_BUNDLE),
                                             MIB, library.MAX_BUNDLE, "max_snapshot_bytes")
    stores = config.get("stores")
    if not isinstance(stores, dict) or not 1 <= len(stores) <= MAX_RECEIPTS:
        raise Error("invalid fixed exporter store map")
    for db_id, source in stores.items():
        library._component(db_id, "db_id")
        _closed(source, ("database", "endpoint", "native_cli", "snapshot_root", "descriptor_revision"),
                "export store", ("database", "endpoint", "native_cli", "snapshot_root"))
        library._component(source["database"], "database")
        library._owner_endpoint(source["endpoint"])
        library._path(source["native_cli"], "native_cli")
        library._path(source["snapshot_root"], "snapshot_root")
        _integer(source.get("descriptor_revision", 1), 1, 2**64 - 1, "descriptor_revision")
    return config


def _export_request(raw):
    if len(raw) > 4096:
        raise Error("cache export request exceeds bound")
    request = _json_output(raw)
    _closed(request, ("schema", "request_id", "db_id", "database", "max_snapshot_bytes"),
            "export request", ("schema", "request_id", "db_id", "database", "max_snapshot_bytes"))
    if request["schema"] != EXPORT_SCHEMA:
        raise Error("wrong cache export request schema")
    if not isinstance(request["request_id"], str) or not re.fullmatch(r"[0-9a-f]{32}", request["request_id"]):
        raise Error("invalid export request identity")
    library._component(request["db_id"], "db_id")
    library._component(request["database"], "database")
    _integer(request["max_snapshot_bytes"], MIB, library.MAX_BUNDLE, "max_snapshot_bytes")
    return request


def _descriptor(config_path, config, source, db_id):
    descriptor = {"project_id": "store_" + db_id, "db_id": db_id,
                  "owner_device_id": config["device_id"], "display_name": db_id,
                  "database": source["database"], "revision": source.get("descriptor_revision", 1),
                  "withdrawn": False}
    path = Path(str(config_path) + ".descriptors.json")
    state = _private_json(path) if path.exists() else {}
    if not isinstance(state, dict) or len(state) > MAX_RECEIPTS:
        raise Error("invalid exporter descriptor history")
    old = state.get(db_id)
    if old is not None:
        if not isinstance(old, dict) or type(old.get("revision")) is not int:
            raise Error("invalid exporter descriptor history")
        same_content = {k: v for k, v in old.items() if k != "revision"} == {
                       k: v for k, v in descriptor.items() if k != "revision"}
        if ((same_content and old["revision"] != descriptor["revision"])
                or (not same_content and descriptor["revision"] <= old["revision"])):
            raise Error("export descriptor change requires a higher revision; unchanged content keeps its revision")
    if old != descriptor:
        if db_id not in state and len(state) >= MAX_RECEIPTS:
            raise Error("export descriptor history exceeds bound")
        state[db_id] = descriptor
        library._write_json(path, state)
    return descriptor


def export(config_path, raw, *, deadline=None):
    """Return one native closed-inventory bundle; stdin cannot choose a route."""
    library._require_relay_support()
    request = _export_request(raw)
    config = _export_config(config_path)
    source = config["stores"].get(request["db_id"])
    if source is None or source["database"] != request["database"]:
        raise Error("export store identity or alias not enrolled")
    deadline = min(deadline, time.monotonic() + config["max_run_secs"]) if deadline is not None else time.monotonic() + config["max_run_secs"]
    maximum = min(request["max_snapshot_bytes"], config["max_snapshot_bytes"])
    roots = [Path(item["snapshot_root"]) for item in config["stores"].values()]
    with _single_worker(config_path):
        descriptor = _descriptor(config_path, config, source, request["db_id"])
        # Native snapshot admission has a fixed 512MiB payload cap, not the
        # caller's lower delivery cap. Reserve that whole capture envelope.
        _reserve(roots, config["max_disk_bytes"], library.MAX_BUNDLE + ARCHIVE_OVERHEAD, deadline)
        bundle = manifest = None
        try:
            bundle, manifest = library.capture_snapshot(source["native_cli"], source["endpoint"],
                    source["database"], request["db_id"], runner=_run, deadline=deadline)
            if (bundle.parent != Path(source["snapshot_root"])
                    or bundle.name != manifest["generation"]):
                bundle = None  # unknown evidence is not ours to remove
                raise Error("native snapshot outside pinned snapshot root")
            size = sum(item["size"] for item in manifest["files"])
            if size > maximum:
                raise Error("native snapshot exceeds configured delivery limit")
            _deadline(deadline)
            data = library._make_archive(descriptor, bundle, request["request_id"],
                                         check=lambda: _deadline(deadline))
            if len(data) > maximum + ARCHIVE_OVERHEAD:
                raise Error("cache export archive exceeds delivery limit")
            _deadline(deadline)
            return data
        finally:
            # Exactly this successful native response, never a root scan. A crash
            # can leave residue; the next capacity check counts it and refuses.
            if bundle is not None:
                shutil.rmtree(bundle)
                library._sync_dir(bundle.parent)


def _worker_config(path):
    config = _common_config(path, "mneme.cache-worker.v1",
            ("schema", "native_cli", "library_config", "peers", "max_disk_bytes", "max_run_secs", "max_transfer_bytes"))
    library._path(config.get("native_cli"), "native_cli")
    library._path(config.get("library_config"), "library_config")
    config["max_transfer_bytes"] = _integer(config.get("max_transfer_bytes", library.MAX_BUNDLE),
                                             MIB, library.MAX_BUNDLE, "max_transfer_bytes")
    peers = config.get("peers")
    if not isinstance(peers, list) or not 1 <= len(peers) <= 32:
        raise Error("invalid cache peers")
    seen = set()
    for peer in peers:
        _closed(peer, ("device_id", "transport", "host", "port", "export_config", "owner_urls"),
                "cache peer", ("device_id", "transport", "owner_urls"))
        library._component(peer["device_id"], "peer device_id")
        urls = peer["owner_urls"]
        if (not isinstance(urls, list) or not 1 <= len(urls) <= MAX_RECEIPTS
                or any(not isinstance(url, str) or not url or len(url) > 4096 for url in urls)):
            raise Error("invalid pinned owner URLs")
        if any(url in seen for url in urls) or len(set(urls)) != len(urls):
            raise Error("owner URL belongs to more than one authenticated peer")
        seen.update(urls)
        if peer["transport"] == "ssh":
            host = library._nonempty(peer.get("host"), "SSH host")
            if host.startswith("-") or any(c.isspace() for c in host) or len(host) > 255:
                raise Error("invalid SSH host")
            _integer(peer.get("port", 22), 1, 65535, "SSH port")
            if "export_config" in peer:
                raise Error("SSH peer cannot select exporter config")
        elif peer["transport"] == "local":
            library._path(peer.get("export_config"), "export_config")
            if "host" in peer or "port" in peer:
                raise Error("local peer cannot select SSH transport")
        else:
            raise Error("invalid cache peer transport")
    return config


def _plan(config, demand, catalog_path, deadline):
    result = _json_output(_run([config["native_cli"], "--json", "cache", "plan", "--demand", str(demand)],
                               b"", 256 * 1024, deadline))
    if (not isinstance(result, dict) or result.get("schema") != "mneme.cache-plan.v1"
            or result.get("demand_path") != str(demand)
            or result.get("catalog_path") != str(catalog_path)
            or result.get("db_id") != demand.stem
            or type(result.get("deadline_unix_secs")) is not int
            or result["deadline_unix_secs"] <= int(time.time())):
        raise Error("native cache plan identity or deadline mismatch")
    library._component(result["db_id"], "plan db_id")
    owner = result.get("owner")
    if not isinstance(owner, dict) or not isinstance(owner.get("url"), str):
        raise Error("native cache plan has no owner")
    library._component(owner.get("db"), "owner database")
    cache = result.get("cache")
    if not isinstance(cache, dict):
        raise Error("native cache plan has no population policy")
    _integer(cache.get("refresh_after_secs"), 60, 86400, "refresh_after_secs")
    _integer(cache.get("max_snapshot_bytes"), MIB, library.MAX_BUNDLE, "max_snapshot_bytes")
    return result


def _binding(plan):
    return {"schema": "mneme.cache-binding.v1", **{key: plan.get(key) for key in
            ("request", "selection_digest", "store", "db_id", "library", "binding", "observed_at_unix_secs")}}


def _fresh(config, catalog, plan, device_id, deadline, receiver_device_id=None, *, allow_stale=False):
    entry = library._entry(catalog, device_id, "store_" + plan["db_id"])
    if entry is None or entry.get("withdrawn") or entry.get("db_id") != plan["db_id"]:
        return False
    replicas = entry.get("replicas", [])
    if not replicas:
        return False
    replica = replicas[0]
    if receiver_device_id is not None and replica.get("source_device_id") != receiver_device_id:
        return False
    if type(replica.get("captured_at")) is not int:
        return False
    age = int(time.time()) - replica["captured_at"]
    if age < 0 or (not allow_stale and age >= plan["cache"]["refresh_after_secs"]):
        return False
    expected = library._replica_path(config["library_config"], device_id, entry["project_id"], replica["generation"])
    if (replica.get("resolved_path") != str(expected)
            or replica.get("database") != library.replica_alias(device_id, entry["project_id"], replica["generation"])
            or not expected.is_file() or expected.is_symlink()):
        return False
    try:
        manifest = library.validate_bundle(expected.parent.parent / "bundle", plan["db_id"], check=lambda: _deadline(deadline))
        if manifest["generation"] != replica["generation"] or manifest["captured_at"] != replica["captured_at"]:
            return False
    except (Error, OSError):
        return False
    return True


def _fetch(peer, request, limit, deadline):
    if peer["transport"] == "ssh":
        argv = ["ssh", "-T", "-oBatchMode=yes", "-oStrictHostKeyChecking=yes",
                "-oConnectTimeout=10", "-p", str(peer.get("port", 22)), "--", peer["host"],
                "exec ~/.local/bin/mneme-cache-export"]
    else:
        argv = [sys.executable, str(Path(__file__).resolve()), "--config", peer["export_config"], "export"]
    return _run(argv, json.dumps(request, separators=(",", ":")).encode(), limit, deadline)


def once(config_path):
    library._require_relay_support()
    config = _worker_config(config_path)
    deadline = time.monotonic() + config["max_run_secs"]
    cp, local, catalog_path, catalog, _, publisher = library._load(config["library_config"])
    if not isinstance(publisher.get("serving"), dict):
        raise Error("cache worker requires fixed local serving stop/start commands")
    demand_dir = catalog_path.parent / "cache-demand"
    if not demand_dir.exists():
        return {"schema": "mneme.cache-run.v1", "result": "idle", "examined": 0, "transfers": 0}
    _private_directory(demand_dir)
    _private_directory(cp.parent)
    with _single_worker(config_path):
        demands = []
        invalid = count = 0
        for path in demand_dir.iterdir():
            if path.suffix != ".json":
                continue
            count += 1
            if count > MAX_RECEIPTS:
                raise Error("cache demand inventory exceeds bound")
            try:
                _private_json(path, MAX_DEMAND)
                library._component(path.stem, "demand identity")
                demands.append(path)
            except (Error, OSError):
                invalid += 1
        state_path = cp.parent / "cache-population.json"
        state = _private_json(state_path) if state_path.exists() else {}
        if (not isinstance(state, dict) or len(state) > MAX_RECEIPTS
                or any(not isinstance(value, dict) or type(value.get("attempted_at")) is not int
                       for value in state.values())):
            raise Error("invalid cache population state")
        demands.sort(key=lambda path: (state.get(path.stem, {}).get("attempted_at", 0), path.name))
        examined = skipped = 0
        for demand in demands:
            _deadline(deadline)
            examined += 1
            previous = state.get(demand.stem, {})
            if int(time.time()) - previous.get("attempted_at", 0) < 60:
                skipped += 1
                continue
            transfer_attempted = False
            phase = "selection"
            try:
                plan = _plan(config, demand, catalog_path, deadline)
                peer = next((peer for peer in config["peers"] if plan["owner"]["url"] in peer["owner_urls"]), None)
                if peer is None or not any(p.get("device_id") == peer["device_id"] for p in publisher["peers"]):
                    raise Error("cache owner has no pinned authenticated peer")
                repair_stale = (previous.get("result") in ("in_progress", "deferred")
                                and previous.get("phase") in ("publication", "serving"))
                if _fresh(config, catalog, plan, peer["device_id"], deadline, local["device_id"],
                          allow_stale=repair_stale):
                    if previous.get("result") not in ("activated", "stale", "serving_restarted"):
                        # A prior crash/start failure may have selected a valid
                        # generation but left its host stopped. Repair serving,
                        # not freshness via yet another cold capture.
                        phase = "serving"
                        with library._lock(cp, deadline=deadline):
                            _, actual, actual_path, actual_catalog, _, actual_publisher = library._load(cp)
                            if actual_path != catalog_path or actual["device_id"] != local["device_id"]:
                                raise Error("cache receiver binding changed")
                            if _plan(config, demand, catalog_path, deadline) != plan:
                                raise Error("cache demand changed before serving repair")
                            if not _fresh(config, actual_catalog, plan, peer["device_id"], deadline, local["device_id"],
                                          allow_stale=repair_stale):
                                raise Error("cache generation changed before serving repair")
                            library._service_command(actual_publisher, "start", deadline=deadline)
                        state[demand.stem] = {"attempted_at": int(time.time()), "result": "serving_restarted"}
                        library._write_json(state_path, state)
                        return {"schema": "mneme.cache-run.v1", "result": "serving_restarted",
                                "db_id": plan["db_id"], "examined": examined, "transfers": 0}
                    skipped += 1
                    continue
                maximum = min(plan["cache"]["max_snapshot_bytes"], config["max_transfer_bytes"] - ARCHIVE_OVERHEAD)
                if maximum < MIB:
                    raise Error("transfer budget is smaller than minimum snapshot envelope")
                # Existing generations (including two retained bundle+serve
                # copies), abandoned stages and metadata are counted. Reserve
                # archive + new stage/bundle + serving copy before capture.
                phase = "capacity"
                _reserve([cp.parent], config["max_disk_bytes"], 3 * maximum + 4 * MIB, deadline)
                request_id = uuid.uuid4().hex
                request = {"schema": EXPORT_SCHEMA, "request_id": request_id, "db_id": plan["db_id"],
                           "database": plan["owner"]["db"], "max_snapshot_bytes": maximum}
                state[demand.stem] = {"attempted_at": int(time.time()), "result": "in_progress", "phase": "transfer"}
                state = {key: value for key, value in state.items() if key in {path.stem for path in demands}}
                library._write_json(state_path, state)
                transfer_attempted = True
                phase = "transfer"
                raw = _fetch(peer, request, maximum + ARCHIVE_OVERHEAD, deadline)
                def replay():
                    if _plan(config, demand, catalog_path, deadline) != plan:
                        raise Error("cache demand or exact selection changed during capture")
                phase = "publication"
                replay()
                state[demand.stem]["phase"] = "publication"
                library._write_json(state_path, state)
                result = library.receive(cp, raw, authenticated_peer=peer["device_id"],
                            expected_db_id=plan["db_id"], expected_delivery_id=request_id,
                            before_publish=replay, binding=_binding(plan), deadline=deadline,
                            expected_project_id="store_" + plan["db_id"],
                            expected_database=plan["owner"]["db"], require_snapshot=True,
                            expected_catalog_path=catalog_path, expected_receiver_device_id=local["device_id"],
                            max_snapshot_bytes=maximum)
                state[demand.stem] = {"attempted_at": int(time.time()), "result": result}
                state = {key: value for key, value in state.items() if key in {path.stem for path in demands}}
                library._write_json(state_path, state)
                return {"schema": "mneme.cache-run.v1", "result": result, "db_id": plan["db_id"],
                        "examined": examined, "transfers": 1, "bytes": len(raw)}
            except (Error, OSError, TimeoutError, subprocess.SubprocessError):
                # A failed attempt cannot starve all later stores forever. No
                # query, payload, credential, or arbitrary subprocess text logs.
                state[demand.stem] = {"attempted_at": int(time.time()), "result": "deferred", "phase": phase}
                state = {key: value for key, value in state.items() if key in {path.stem for path in demands}}
                library._write_json(state_path, state)
                return {"schema": "mneme.cache-run.v1", "result": "deferred", "db_id": demand.stem,
                        "examined": examined, "transfers": int(transfer_attempted), "phase": phase}
        return {"schema": "mneme.cache-run.v1", "result": "idle", "examined": examined,
                "skipped": skipped, "invalid": invalid, "transfers": 0}


def _read_export(deadline):
    descriptor = sys.stdin.fileno()
    if stat.S_ISREG(os.fstat(descriptor).st_mode):
        return os.read(descriptor, 4097)
    os.set_blocking(descriptor, False)
    data = bytearray()
    with selectors.DefaultSelector() as selector:
        selector.register(descriptor, selectors.EVENT_READ)
        while True:
            _deadline(deadline)
            if not selector.select(max(0.001, deadline - time.monotonic())):
                raise Error("cache export request deadline exceeded")
            try:
                chunk = os.read(descriptor, 4097 - len(data))
            except BlockingIOError:
                continue
            if not chunk:
                return bytes(data)
            data.extend(chunk)
            if len(data) > 4096:
                raise Error("cache export request exceeds bound")


def _write_export(data, deadline):
    # A stalled SSH consumer must not retain an exporter indefinitely either.
    descriptor = sys.stdout.fileno()
    regular = stat.S_ISREG(os.fstat(descriptor).st_mode)
    os.set_blocking(descriptor, False)
    with selectors.DefaultSelector() as selector:
        if not regular:
            selector.register(descriptor, selectors.EVENT_WRITE)
        pending = memoryview(data)
        while pending:
            _deadline(deadline)
            if not regular and not selector.select(max(0.001, deadline - time.monotonic())):
                raise Error("cache export output deadline exceeded")
            try:
                count = os.write(descriptor, pending[:64 * 1024])
            except BlockingIOError:
                continue
            if count <= 0:
                raise Error("cache export output closed")
            pending = pending[count:]


def main(argv=None):
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--config", required=True, type=Path)
    parser.add_argument("command", choices=("once", "export"))
    args = parser.parse_args(argv)
    try:
        if args.command == "export":
            config = _export_config(args.config)
            deadline = time.monotonic() + config["max_run_secs"]
            raw = _read_export(min(deadline, time.monotonic() + 5))
            _write_export(export(args.config, raw, deadline=deadline), deadline)
        else:
            print(json.dumps(once(args.config), sort_keys=True))
        return 0
    except (Error, OSError, TimeoutError, subprocess.SubprocessError):
        # Config/paths and subprocess stderr may include private owner details.
        print("cache operation refused; verify fixed config, native selection, capacity, and owner availability", file=sys.stderr)
        return 1


if __name__ == "__main__":
    raise SystemExit(main())
