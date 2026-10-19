"""Disposable boundary tests: no owner process, live store, or enrolled config."""
import copy
import hashlib
import io
import json
import os
from pathlib import Path
import subprocess
import sys
import tarfile
import tempfile
import time
import unittest
from unittest.mock import patch

sys.path.insert(0, str(Path(__file__).parent))
import library

if os.name == "posix":
    import cache
else:
    cache = None


DB = "01M3CPEVDTM9PP4HJTMMBZ8HR9"
OTHER_DB = "01M3CPEVDTM9PP4HJTMMBZ8HRA"


def private_json(path, value):
    path.parent.mkdir(mode=0o700, parents=True, exist_ok=True)
    path.write_text(json.dumps(value))
    path.chmod(0o600)


@unittest.skipUnless(os.name == "posix", "cache publication requires Unix durability")
class CacheBoundaryTests(unittest.TestCase):
    def setUp(self):
        temporary = tempfile.TemporaryDirectory()
        self.addCleanup(temporary.cleanup)
        self.root = Path(temporary.name).resolve()
        self.root.chmod(0o700)
        self.local = self.root / "local"
        self.local.mkdir(mode=0o700)
        self.config = self.local / "library.json"
        self.catalog = self.local / "catalog.json"
        private_json(self.config, {"schema": library.CONFIG_SCHEMA,
            "library_id": "reference", "device_id": "local", "catalog_path": "catalog.json",
            "owners": {}, "replicas": {}})
        private_json(self.catalog, {"schema": library.SCHEMA, "library_id": "reference",
            "revision": 0, "entries": []})
        private_json(library._publisher_path(self.config), {"schema": library.PUBLISHER_SCHEMA,
            "peers": [{"device_id": "remote", "transport": "ssh"}],
            "sources": {}, "outbox": [], "serving": {
                "stop": [sys.executable, "-c", "pass"],
                "start": [sys.executable, "-c", "pass"]}})
        self.worker_config = self.local / "worker.json"
        self.worker = {"schema": "mneme.cache-worker.v1",
            "native_cli": str(self.root / "mnemed"), "library_config": str(self.config),
            "max_disk_bytes": 1024 * cache.MIB, "max_transfer_bytes": 8 * cache.MIB,
            "peers": [{"device_id": "remote", "transport": "ssh", "host": "test-peer",
                       "owner_urls": ["http://127.0.0.1:18001"]}]}
        private_json(self.worker_config, self.worker)
        self.export_root = self.root / "exports"
        self.export_root.mkdir(mode=0o700)
        self.export_config = self.root / "export.json"
        self.exporter = {"schema": "mneme.cache-exporter.v1", "device_id": "remote",
            "max_disk_bytes": 1024 * cache.MIB, "max_snapshot_bytes": 8 * cache.MIB,
            "stores": {DB: {"database": "project", "endpoint": "http://127.0.0.1:18001",
                           "native_cli": str(self.root / "mnemed"),
                           "snapshot_root": str(self.export_root)}}}
        private_json(self.export_config, self.exporter)
        self.now = int(time.time())

    def request(self, **updates):
        value = {"schema": cache.EXPORT_SCHEMA, "request_id": "a" * 32, "db_id": DB,
                 "database": "project", "max_snapshot_bytes": 4 * cache.MIB}
        value.update(updates)
        return value

    def descriptor(self, db_id=DB, **updates):
        value = {"project_id": "store_" + db_id, "db_id": db_id,
                 "owner_device_id": "remote", "display_name": db_id, "database": "project",
                 "revision": 1, "withdrawn": False}
        value.update(updates)
        return value

    def bundle(self, generation="GEN1", *, db_id=DB, captured_at=None, root=None):
        bundle = (root or self.root) / generation
        bundle.mkdir(mode=0o700)
        (bundle / "bodies").mkdir(mode=0o700)
        # These are opaque snapshot bytes, not a database opened by this test.
        # References inside payloads must not become receiver filesystem reads.
        files = {"database.db": b"opaque database with body reference bodies/body1",
                 "bodies/body1": b"opaque note: file:///not/a/local/body; ../outside"}
        inventory = []
        for name, payload in files.items():
            path = bundle / name
            path.write_bytes(payload)
            path.chmod(0o600)
            inventory.append({"path": name, "size": len(payload),
                              "sha256": hashlib.sha256(payload).hexdigest()})
        private_json(bundle / "manifest.json", {"schema": library.SNAPSHOT_SCHEMA,
            "db_id": db_id, "generation": generation,
            "captured_at": self.now if captured_at is None else captured_at,
            "embedding_fingerprint": {}, "files": inventory})
        return bundle

    def archive(self, request, *, generation="GEN1", captured_at=None, db_id=None,
                owner="remote", delivery_id=None):
        db_id = db_id or request["db_id"]
        bundle = self.bundle(generation, db_id=db_id, captured_at=captured_at)
        return library._make_archive(self.descriptor(db_id, owner_device_id=owner), bundle,
                                     delivery_id or request["request_id"])

    def demand(self, db_id=DB):
        path = self.local / "cache-demand" / (db_id + ".json")
        plan = {"schema": "mneme.cache-plan.v1", "demand_path": str(path),
            "request": {"mode": "explicit", "user_path": str(self.root / "memory.toml"),
                        "cwd": str(self.root), "requested_profile": "default",
                        "lexical_pwd": str(self.root), "user_codex_dir": None},
            "selection_digest": "b" * 64, "store": "project", "db_id": db_id,
            "library": "reference", "owner": {"url": "http://127.0.0.1:18001", "db": "project"},
            "cache": {"refresh_after_secs": 900, "max_snapshot_bytes": 4 * cache.MIB},
            "catalog_path": str(self.catalog), "binding": None,
            "observed_at_unix_secs": self.now, "deadline_unix_secs": self.now + 86400}
        record = {key: plan[key] for key in ("request", "selection_digest", "store", "db_id",
                                            "library", "observed_at_unix_secs")}
        record.update(schema="mneme.cache-demand.v1", explicit_store=False)
        private_json(path, record)
        return path, plan

    def fetch_archive(self, peer, request, limit, deadline):
        return self.archive(request)

    def seed_old_generation(self):
        old = self.archive(self.request(), generation="OLD", captured_at=self.now - 1800)
        with patch.object(library, "_service_command"):
            self.assertEqual(library.receive(self.config, old, authenticated_peer="remote"), "activated")
        return self.catalog.read_bytes()

    def test_export_request_is_closed_and_cannot_import_paths_or_routes(self):
        for key, value in (("endpoint", "http://127.0.0.1:1"), ("native_cli", "/bin/false"),
                           ("snapshot_root", "/tmp"), ("path", "/private/store"),
                           ("token_env", "SECRET"), ("owner", {}), ("extra", True)):
            with self.subTest(key=key), patch.object(library, "capture_snapshot") as capture:
                with self.assertRaisesRegex(library.LibraryError, "export request fields"):
                    cache.export(self.export_config, json.dumps(self.request(**{key: value})).encode())
                capture.assert_not_called()

    def test_export_request_rejects_alias_identity_and_duplicate_fields(self):
        for update in ({"database": "other"}, {"db_id": OTHER_DB}):
            with self.subTest(update=update), patch.object(library, "capture_snapshot") as capture:
                with self.assertRaisesRegex(library.LibraryError, "not enrolled"):
                    cache.export(self.export_config, json.dumps(self.request(**update)).encode())
                capture.assert_not_called()
        raw = json.dumps(self.request()).replace('"database": "project"',
                                                 '"database": "project", "database": "other"')
        with self.assertRaisesRegex(library.LibraryError, "duplicate JSON field"):
            cache._export_request(raw.encode())

    def test_export_request_requires_bounded_typed_fields(self):
        for update in ({"request_id": "invalid"}, {"db_id": "../DB"}, {"database": "/path"},
                       {"max_snapshot_bytes": True}, {"max_snapshot_bytes": cache.MIB - 1},
                       {"max_snapshot_bytes": library.MAX_BUNDLE + 1}):
            with self.subTest(update=update), self.assertRaises(library.LibraryError):
                cache._export_request(json.dumps(self.request(**update)).encode())
        with self.assertRaisesRegex(library.LibraryError, "exceeds bound"):
            cache._export_request(b" " * 4097)

    def test_exporter_rejects_nonloopback_and_unpinned_config(self):
        for endpoint in ("http://192.0.2.1:18001", "http://localhost:18001",
                         "https://127.0.0.1:18001", "http://127.0.0.1:18001/mcp"):
            value = copy.deepcopy(self.exporter)
            value["stores"][DB]["endpoint"] = endpoint
            private_json(self.export_config, value)
            with self.subTest(endpoint=endpoint), patch.object(library, "capture_snapshot") as capture:
                with self.assertRaisesRegex(library.LibraryError, "loopback"):
                    cache.export(self.export_config, json.dumps(self.request()).encode())
                capture.assert_not_called()
        value = copy.deepcopy(self.exporter)
        value["stores"][DB]["unknown"] = True
        private_json(self.export_config, value)
        with self.assertRaisesRegex(library.LibraryError, "export store fields"):
            cache._export_config(self.export_config)

    def test_private_config_permissions_and_links_are_enforced(self):
        self.worker_config.chmod(0o644)
        with self.assertRaisesRegex(library.LibraryError, "0600"):
            cache._worker_config(self.worker_config)
        self.worker_config.chmod(0o600)
        self.local.chmod(0o755)
        with self.assertRaisesRegex(library.LibraryError, "0700"):
            cache._worker_config(self.worker_config)
        self.local.chmod(0o700)
        alias = self.local / "alias.json"
        os.link(self.worker_config, alias)
        with self.assertRaisesRegex(library.LibraryError, "singly linked"):
            cache._worker_config(self.worker_config)

    def test_export_uses_only_pinned_native_route_and_removes_exact_capture(self):
        bundle = self.bundle(root=self.export_root)
        residue = self.export_root / "unrecognized"
        residue.mkdir(mode=0o700)
        response = json.dumps({"db_id": DB, "bundle": str(bundle), "generation": "GEN1"}).encode()
        with patch.object(cache, "_run", return_value=response) as run:
            raw = cache.export(self.export_config, json.dumps(self.request()).encode())
        argv, payload, bound, deadline = run.call_args.args
        self.assertEqual(argv, [str(self.root / "mnemed"), "--json", "--remote",
            "http://127.0.0.1:18001", "--remote-db", "project", "snapshot", "create",
            "--expected-db-id", DB])
        self.assertEqual(payload, b"")
        self.assertEqual(bound, library.MAX_JSON)
        self.assertGreater(deadline, time.monotonic())
        self.assertFalse(bundle.exists())
        self.assertTrue(residue.is_dir())
        with tarfile.open(fileobj=io.BytesIO(raw), mode="r:") as archive:
            message = json.load(archive.extractfile("message.json"))
            self.assertEqual(message["delivery_id"], self.request()["request_id"])
            self.assertEqual(message["descriptor"]["db_id"], DB)
            self.assertEqual(archive.extractfile("snapshot/bodies/body1").read(),
                             b"opaque note: file:///not/a/local/body; ../outside")

    def test_export_preserves_capture_outside_pinned_root(self):
        bundle = self.bundle()
        response = json.dumps({"db_id": DB, "bundle": str(bundle), "generation": "GEN1"}).encode()
        with patch.object(cache, "_run", return_value=response):
            with self.assertRaisesRegex(library.LibraryError, "outside pinned"):
                cache.export(self.export_config, json.dumps(self.request()).encode())
        self.assertTrue(bundle.is_dir())

    def test_export_capacity_is_checked_before_capture(self):
        self.exporter["max_disk_bytes"] = cache.MIB
        private_json(self.export_config, self.exporter)
        with patch.object(library, "capture_snapshot") as capture:
            with self.assertRaisesRegex(library.LibraryError, "capacity"):
                cache.export(self.export_config, json.dumps(self.request()).encode())
            capture.assert_not_called()

    def test_exporter_empty_map_is_not_authority_to_enumerate_or_capture(self):
        self.exporter["stores"] = {}
        private_json(self.export_config, self.exporter)
        with patch.object(library, "capture_snapshot") as capture:
            with self.assertRaisesRegex(library.LibraryError, "fixed exporter store map"):
                cache.export(self.export_config, json.dumps(self.request()).encode())
            capture.assert_not_called()

    def test_export_alias_change_requires_higher_persisted_descriptor_revision(self):
        def native(argv, payload, bound, deadline):
            bundle = self.bundle(root=self.export_root)
            return json.dumps({"db_id": DB, "bundle": str(bundle), "generation": "GEN1"}).encode()
        with patch.object(cache, "_run", side_effect=native):
            cache.export(self.export_config, json.dumps(self.request()).encode())
        history_path = Path(str(self.export_config) + ".descriptors.json")
        history = json.loads(history_path.read_text())
        self.assertEqual(history[DB]["revision"], 1)
        self.assertEqual(history_path.stat().st_mode & 0o777, 0o600)
        self.exporter["stores"][DB]["database"] = "renamed"
        private_json(self.export_config, self.exporter)
        with patch.object(library, "capture_snapshot") as capture:
            with self.assertRaisesRegex(library.LibraryError, "higher revision"):
                cache.export(self.export_config, json.dumps(self.request(database="renamed")).encode())
            capture.assert_not_called()
        self.assertEqual(json.loads(history_path.read_text()), history)
        self.exporter["stores"][DB]["descriptor_revision"] = 2
        private_json(self.export_config, self.exporter)
        with patch.object(cache, "_run", side_effect=native):
            raw = cache.export(self.export_config, json.dumps(self.request(database="renamed")).encode())
        with tarfile.open(fileobj=io.BytesIO(raw), mode="r:") as archive:
            descriptor = json.load(archive.extractfile("message.json"))["descriptor"]
        self.assertEqual((descriptor["database"], descriptor["revision"]), ("renamed", 2))
        self.assertEqual(json.loads(history_path.read_text())[DB], descriptor)

    def test_invalid_tar_is_a_deferred_worker_failure_without_catalog_change(self):
        _, plan = self.demand()
        before = self.catalog.read_bytes()
        with patch.object(cache, "_plan", return_value=plan), \
                patch.object(cache, "_fetch", return_value=b"not a tar archive"):
            result = cache.once(self.worker_config)
        self.assertEqual(result["result"], "deferred")
        self.assertEqual(result["phase"], "publication")
        self.assertEqual(result["transfers"], 1)
        self.assertEqual(self.catalog.read_bytes(), before)

    def test_receive_binds_expected_database_and_delivery_before_publication(self):
        raw = self.archive(self.request())
        old = self.catalog.read_bytes()
        for expected in ({"expected_db_id": OTHER_DB}, {"expected_delivery_id": "c" * 32}):
            with self.subTest(expected=expected), patch.object(library, "_service_command") as service:
                with self.assertRaisesRegex(library.LibraryError, "unexpected"):
                    library.receive(self.config, raw, authenticated_peer="remote", **expected)
                service.assert_not_called()
                self.assertEqual(self.catalog.read_bytes(), old)
                self.assertEqual(list((self.local / "library-replicas").iterdir()), [])

    def test_receive_rejects_authenticated_peer_owner_spoof(self):
        raw = self.archive(self.request(), owner="somebody-else")
        old = self.catalog.read_bytes()
        with patch.object(library, "_service_command") as service:
            with self.assertRaisesRegex(library.LibraryError, "authenticated peer"):
                library.receive(self.config, raw, authenticated_peer="remote", expected_db_id=DB)
            service.assert_not_called()
        self.assertEqual(self.catalog.read_bytes(), old)

    def test_worker_without_demand_never_plans_or_exports(self):
        for create_empty in (False, True):
            if create_empty:
                (self.local / "cache-demand").mkdir(mode=0o700)
            with self.subTest(create_empty=create_empty), patch.object(cache, "_plan") as plan, \
                    patch.object(cache, "_fetch") as fetch:
                result = cache.once(self.worker_config)
                self.assertEqual(result["result"], "idle")
                self.assertEqual(result["transfers"], 0)
                plan.assert_not_called()
                fetch.assert_not_called()

    def test_worker_one_exact_request_one_transfer_and_bound_publication(self):
        demand, plan = self.demand()
        self.demand(OTHER_DB)
        publisher_before = library._publisher_path(self.config).read_bytes()
        config_before = self.config.read_bytes()
        with patch.object(cache, "_plan", return_value=plan) as native, \
                patch.object(cache, "_fetch", side_effect=self.fetch_archive) as fetch, \
                patch.object(library, "_service_command") as service:
            result = cache.once(self.worker_config)
        self.assertEqual(result["result"], "activated")
        self.assertEqual((result["examined"], result["transfers"]), (1, 1))
        self.assertEqual(native.call_count, 4)  # admission, post-fetch, pre-stop, pre-selector
        self.assertTrue(all(call.args[1] == demand for call in native.call_args_list))
        fetch.assert_called_once()
        peer, request, limit, deadline = fetch.call_args.args
        self.assertEqual(set(request), {"schema", "request_id", "db_id", "database", "max_snapshot_bytes"})
        self.assertEqual((request["db_id"], request["database"]), (DB, "project"))
        self.assertEqual(limit, request["max_snapshot_bytes"] + cache.ARCHIVE_OVERHEAD)
        self.assertEqual(peer["device_id"], "remote")
        self.assertGreater(deadline, time.monotonic())
        self.assertEqual([call.args[1] for call in service.call_args_list], ["stop", "start"])
        catalog = json.loads(self.catalog.read_text())
        self.assertEqual(len(catalog["entries"]), 1)
        replica = catalog["entries"][0]["replicas"][0]
        serving = Path(replica["resolved_path"])
        generation = serving.parent.parent
        self.assertEqual(json.loads((generation / "cache-binding.json").read_text()), cache._binding(plan))
        manifest = library.validate_bundle(generation / "bundle", DB)
        for item in manifest["files"]:
            relative = item["path"].replace("bodies/", "database.bodies/")
            payload = (serving.parent / relative).read_bytes()
            self.assertEqual(hashlib.sha256(payload).hexdigest(), item["sha256"])
        self.assertEqual(self.config.read_bytes(), config_before)
        self.assertEqual(library._publisher_path(self.config).read_bytes(), publisher_before)

    def test_worker_stale_initial_plan_never_exports(self):
        self.demand()
        old = self.seed_old_generation()
        with patch.object(cache, "_plan", side_effect=library.LibraryError("stale demand")), \
                patch.object(cache, "_fetch") as fetch:
            result = cache.once(self.worker_config)
        self.assertEqual((result["result"], result["transfers"]), ("deferred", 0))
        fetch.assert_not_called()
        self.assertEqual(self.catalog.read_bytes(), old)

    def test_changed_plan_after_transfer_or_before_selector_keeps_old_catalog(self):
        _, plan = self.demand()
        old = self.seed_old_generation()
        changed = copy.deepcopy(plan)
        changed["selection_digest"] = "c" * 64
        for late in (False, True):
            state = self.local / "cache-population.json"
            state.unlink(missing_ok=True)
            generation = "NEW_LATE" if late else "NEW_EARLY"
            def fetch(peer, request, limit, deadline):
                return self.archive(request, generation=generation)
            plans = [plan, plan, changed] if late else [plan, changed]
            with self.subTest(late=late), patch.object(cache, "_plan", side_effect=plans), \
                    patch.object(cache, "_fetch", side_effect=fetch) as transfer, \
                    patch.object(library, "_service_command") as service:
                result = cache.once(self.worker_config)
            self.assertEqual((result["result"], result["transfers"]), ("deferred", 1))
            transfer.assert_called_once()
            service.assert_not_called()
            self.assertEqual(self.catalog.read_bytes(), old)
            self.assertTrue(Path(json.loads(old)["entries"][0]["replicas"][0]["resolved_path"]).is_file())

    def test_policy_change_while_old_host_stops_restarts_old_without_publication(self):
        _, plan = self.demand()
        old = self.seed_old_generation()
        changed = copy.deepcopy(plan)
        changed["selection_digest"] = "c" * 64
        with patch.object(cache, "_plan", side_effect=[plan, plan, plan, changed]), \
                patch.object(cache, "_fetch", side_effect=self.fetch_archive), \
                patch.object(library, "_service_command") as service:
            result = cache.once(self.worker_config)
        self.assertEqual(result["result"], "deferred")
        self.assertEqual(self.catalog.read_bytes(), old)
        self.assertEqual([call.args[1] for call in service.call_args_list], ["stop", "start"])

    def test_failed_publication_repairs_stale_selected_host_before_new_capture(self):
        _, plan = self.demand()
        old = self.seed_old_generation()
        private_json(self.local / "cache-population.json", {DB: {
            "attempted_at": self.now - 61, "result": "deferred", "phase": "publication"}})
        with patch.object(cache, "_plan", return_value=plan), \
                patch.object(cache, "_fetch") as fetch, \
                patch.object(library, "_service_command") as service:
            result = cache.once(self.worker_config)
        self.assertEqual(result["result"], "serving_restarted")
        self.assertEqual(result["transfers"], 0)
        fetch.assert_not_called()
        self.assertEqual([call.args[1] for call in service.call_args_list], ["start"])
        self.assertEqual(self.catalog.read_bytes(), old)

    def test_worker_capacity_exhaustion_never_exports_or_deletes_old_generation(self):
        _, plan = self.demand()
        old = self.seed_old_generation()
        self.worker["max_disk_bytes"] = cache.MIB
        private_json(self.worker_config, self.worker)
        with patch.object(cache, "_plan", return_value=plan), patch.object(cache, "_fetch") as fetch:
            result = cache.once(self.worker_config)
        self.assertEqual((result["result"], result["transfers"]), ("deferred", 0))
        fetch.assert_not_called()
        self.assertEqual(self.catalog.read_bytes(), old)

    def test_busy_owner_is_deferred_once_and_throttled_without_retry(self):
        _, plan = self.demand()
        with patch.object(cache, "_plan", return_value=plan) as native, \
                patch.object(cache, "_fetch", side_effect=library.LibraryError("busy")) as fetch:
            first = cache.once(self.worker_config)
            second = cache.once(self.worker_config)
        self.assertEqual((first["result"], first["transfers"]), ("deferred", 1))
        self.assertEqual((second["result"], second["transfers"]), ("idle", 0))
        fetch.assert_called_once()
        native.assert_called_once()

    def test_worker_rejects_unpinned_owner_before_transfer(self):
        _, plan = self.demand()
        plan["owner"]["url"] = "http://127.0.0.1:19999"
        with patch.object(cache, "_plan", return_value=plan), patch.object(cache, "_fetch") as fetch:
            self.assertEqual(cache.once(self.worker_config)["result"], "deferred")
        fetch.assert_not_called()

    def test_worker_does_not_trust_response_database_delivery_or_owner(self):
        _, plan = self.demand()
        old = self.seed_old_generation()
        for index, updates in enumerate(({"db_id": OTHER_DB}, {"delivery_id": "f" * 32},
                                         {"owner": "somebody-else"})):
            (self.local / "cache-population.json").unlink(missing_ok=True)
            def fetch(peer, request, limit, deadline):
                return self.archive(request, generation="WRONG" + str(index), **updates)
            with self.subTest(updates=updates), patch.object(cache, "_plan", return_value=plan), \
                    patch.object(cache, "_fetch", side_effect=fetch), \
                    patch.object(library, "_service_command") as service:
                result = cache.once(self.worker_config)
            self.assertEqual((result["result"], result["transfers"]), ("deferred", 1))
            service.assert_not_called()
            self.assertEqual(self.catalog.read_bytes(), old)

    def test_fresh_validated_generation_suppresses_export(self):
        _, plan = self.demand()
        with patch.object(cache, "_plan", return_value=plan), \
                patch.object(cache, "_fetch", side_effect=self.fetch_archive) as fetch, \
                patch.object(library, "_service_command"):
            self.assertEqual(cache.once(self.worker_config)["result"], "activated")
            state_path = self.local / "cache-population.json"
            state = json.loads(state_path.read_text())
            state[DB]["attempted_at"] = self.now - 61
            private_json(state_path, state)
            self.assertEqual(cache.once(self.worker_config)["result"], "idle")
        fetch.assert_called_once()

    def test_freshness_checks_immutable_bundle_and_serving_presence_not_mutable_database_bytes(self):
        _, plan = self.demand()
        with patch.object(cache, "_plan", return_value=plan), \
                patch.object(cache, "_fetch", side_effect=self.fetch_archive), \
                patch.object(library, "_service_command"):
            self.assertEqual(cache.once(self.worker_config)["result"], "activated")
        catalog = json.loads(self.catalog.read_text())
        serving = Path(catalog["entries"][0]["replicas"][0]["resolved_path"])
        def fresh():
            return cache._fresh(self.worker, catalog, plan, "remote", time.monotonic() + 3,
                                receiver_device_id="local")
        self.assertTrue(fresh())
        # The native backend can update metadata in its private serving copy.
        # Immutable capture evidence, not that mutable copy, owns the hash proof.
        serving.write_bytes(b"legitimate opened-backend metadata differs")
        self.assertTrue(fresh())
        serving.unlink()
        self.assertFalse(fresh())
        serving.write_bytes(b"private serving database")
        serving.chmod(0o600)
        body = serving.parent.parent / "bundle" / "bodies" / "body1"
        body.write_bytes(b"corrupted capture evidence")
        self.assertFalse(fresh())

    def test_failed_serving_start_repairs_fresh_generation_without_another_capture(self):
        _, plan = self.demand()
        starts = 0
        def service(publisher, action, **kwargs):
            nonlocal starts
            if action == "start":
                starts += 1
                if starts == 1:
                    raise library.LibraryError("serving restart failed")
        with patch.object(cache, "_plan", return_value=plan), \
                patch.object(cache, "_fetch", side_effect=self.fetch_archive) as fetch, \
                patch.object(library, "_service_command", side_effect=service) as serving:
            first = cache.once(self.worker_config)
            self.assertEqual((first["result"], first["transfers"]), ("deferred", 1))
            selected = self.catalog.read_bytes()
            self.assertEqual(len(json.loads(selected)["entries"]), 1)
            state_path = self.local / "cache-population.json"
            state = json.loads(state_path.read_text())
            state[DB]["attempted_at"] = self.now - 61
            private_json(state_path, state)
            second = cache.once(self.worker_config)
        self.assertEqual((second["result"], second["transfers"]), ("serving_restarted", 0))
        self.assertEqual(self.catalog.read_bytes(), selected)
        self.assertEqual([call.args[1] for call in serving.call_args_list], ["stop", "start", "start"])
        fetch.assert_called_once()

    def test_worker_records_in_progress_before_external_transfer(self):
        _, plan = self.demand()
        def fail_after_start(peer, request, limit, deadline):
            state = json.loads((self.local / "cache-population.json").read_text())
            self.assertEqual(state[DB]["result"], "in_progress")
            raise library.LibraryError("unavailable")
        with patch.object(cache, "_plan", return_value=plan), \
                patch.object(cache, "_fetch", side_effect=fail_after_start):
            self.assertEqual(cache.once(self.worker_config)["result"], "deferred")

    def test_receiver_binding_change_during_fetch_prevents_publication(self):
        _, plan = self.demand()
        old = self.seed_old_generation()
        original = json.loads(self.config.read_text())
        foreign_catalog = self.local / "different-catalog.json"
        for index, mutation in enumerate(({"device_id": "different"},
                                         {"catalog_path": str(foreign_catalog)})):
            private_json(self.config, original)
            (self.local / "cache-population.json").unlink(missing_ok=True)
            def fetch(peer, request, limit, deadline):
                raw = self.archive(request, generation="REBIND" + str(index))
                private_json(self.config, dict(original, **mutation))
                return raw
            with self.subTest(mutation=mutation), patch.object(cache, "_plan", return_value=plan), \
                    patch.object(cache, "_fetch", side_effect=fetch), \
                    patch.object(library, "_service_command") as service:
                result = cache.once(self.worker_config)
            self.assertEqual((result["result"], result["transfers"]), ("deferred", 1))
            service.assert_not_called()
            self.assertEqual(self.catalog.read_bytes(), old)
            self.assertFalse(foreign_catalog.exists())

    def test_receive_lock_wait_is_bounded_by_operation_deadline(self):
        raw = self.archive(self.request())
        old = self.catalog.read_bytes()
        started = time.monotonic()
        with library._lock(self.config):
            with self.assertRaisesRegex(library.LibraryError, "lock deadline"):
                library.receive(self.config, raw, authenticated_peer="remote",
                                deadline=time.monotonic() + 0.1)
        self.assertLess(time.monotonic() - started, 1)
        self.assertEqual(self.catalog.read_bytes(), old)
        self.assertFalse((self.local / "library-replicas").exists())

    def test_receive_requires_snapshot_and_expected_alias_project(self):
        bundle = self.bundle()
        descriptor = self.descriptor()
        raw = library._make_archive(descriptor, bundle, "a" * 32)
        old = self.catalog.read_bytes()
        for expected in ({"expected_project_id": "different"}, {"expected_database": "different"}):
            with self.subTest(expected=expected), self.assertRaisesRegex(library.LibraryError, "unexpected"):
                library.receive(self.config, raw, authenticated_peer="remote", **expected)
            self.assertEqual(self.catalog.read_bytes(), old)
        raw = library._make_archive(descriptor, None, "a" * 32)
        with self.assertRaisesRegex(library.LibraryError, "snapshot"):
            library.receive(self.config, raw, authenticated_peer="remote", require_snapshot=True)
        self.assertEqual(self.catalog.read_bytes(), old)

    def test_receive_snapshot_payload_cap_keeps_old_catalog_and_generation(self):
        old = self.seed_old_generation()
        raw = self.archive(self.request())
        manifest = json.loads((self.root / "GEN1" / "manifest.json").read_text())
        cap = sum(item["size"] for item in manifest["files"]) - 1
        with patch.object(library, "_service_command") as service:
            with self.assertRaisesRegex(library.LibraryError, "requested receive bound"):
                library.receive(self.config, raw, authenticated_peer="remote", max_snapshot_bytes=cap)
            service.assert_not_called()
        self.assertEqual(self.catalog.read_bytes(), old)
        old_path = Path(json.loads(old)["entries"][0]["replicas"][0]["resolved_path"])
        self.assertTrue(old_path.is_file())
        self.assertFalse((old_path.parent.parent.parent / "GEN1").exists())

    def test_extract_enforces_member_count_incrementally_before_next_payload(self):
        output = io.BytesIO()
        with tarfile.open(fileobj=output, mode="w") as archive:
            for name in ("message.json", "snapshot/manifest.json", "snapshot/database.db", "snapshot/extra"):
                member = tarfile.TarInfo(name)
                member.size = 1
                archive.addfile(member, io.BytesIO(b"x"))
        stage = self.root / "stage"
        stage.mkdir(mode=0o700)
        with patch.object(library, "MAX_FILES", 1), \
                patch.object(library, "_write_stream", wraps=library._write_stream) as writes:
            with self.assertRaisesRegex(library.LibraryError, "archive inventory"):
                library._extract(output.getvalue(), stage)
        self.assertEqual(writes.call_count, 3)
        self.assertFalse((stage / "snapshot" / "extra").exists())

    def test_corrupted_archive_is_rejected_without_catalog_publication(self):
        raw = self.archive(self.request())
        changed = io.BytesIO()
        with tarfile.open(fileobj=io.BytesIO(raw), mode="r:") as source, \
                tarfile.open(fileobj=changed, mode="w") as destination:
            for member in source:
                payload = source.extractfile(member).read()
                if member.name == "snapshot/database.db":
                    payload = b"x" * len(payload)
                destination.addfile(member, io.BytesIO(payload))
        old = self.catalog.read_bytes()
        with patch.object(library, "_service_command") as service:
            with self.assertRaisesRegex(library.LibraryError, "checksum"):
                library.receive(self.config, changed.getvalue(), authenticated_peer="remote")
            service.assert_not_called()
        self.assertEqual(self.catalog.read_bytes(), old)

    def test_native_plan_rejects_stale_or_wrong_response_identity(self):
        demand, plan = self.demand()
        for update in ({"deadline_unix_secs": self.now - 1}, {"db_id": OTHER_DB},
                       {"catalog_path": str(self.root / "foreign-catalog.json")},
                       {"demand_path": str(self.root / "foreign-demand.json")}):
            response = dict(plan, **update)
            with self.subTest(update=update), patch.object(cache, "_run", return_value=json.dumps(response).encode()):
                with self.assertRaisesRegex(library.LibraryError, "identity or deadline"):
                    cache._plan(self.worker, demand, self.catalog, time.monotonic() + 3)

    def test_peer_config_has_single_owner_and_no_remote_exporter_override(self):
        for mutation in ("duplicate", "export_config", "option_host"):
            value = copy.deepcopy(self.worker)
            if mutation == "duplicate":
                value["peers"].append(dict(value["peers"][0], device_id="different"))
            elif mutation == "export_config":
                value["peers"][0]["export_config"] = "/tmp/foreign.json"
            else:
                value["peers"][0]["host"] = "-oProxyCommand=anything"
            private_json(self.worker_config, value)
            with self.subTest(mutation=mutation), self.assertRaises(library.LibraryError):
                cache._worker_config(self.worker_config)

    def test_worker_lock_never_queues_another_capture(self):
        self.demand()
        with cache._single_worker(self.worker_config), patch.object(cache, "_plan") as plan, \
                patch.object(cache, "_fetch") as fetch:
            with self.assertRaisesRegex(library.LibraryError, "already active"):
                cache.once(self.worker_config)
        plan.assert_not_called()
        fetch.assert_not_called()

    def test_subprocess_runner_cuts_off_output_before_collecting_a_flood(self):
        started = time.monotonic()
        with self.assertRaisesRegex(library.LibraryError, "output exceeds bound"):
            cache._run([sys.executable, "-c", "import os;\nwhile True: os.write(1, b'x' * 65536)"],
                       b"", 8192, time.monotonic() + 3)
        self.assertLess(time.monotonic() - started, 3)

    def test_subprocess_runner_deadline_terminates_a_silent_child(self):
        started = time.monotonic()
        with self.assertRaises((library.LibraryError, TimeoutError, subprocess.TimeoutExpired)):
            cache._run([sys.executable, "-c", "import time; time.sleep(60)"],
                       b"", 8192, time.monotonic() + 0.2)
        self.assertLess(time.monotonic() - started, 3)

    def test_subprocess_nonzero_exit_is_not_retried(self):
        with patch.object(cache.host_process, "spawn", wraps=cache.host_process.spawn) as spawn:
            with self.assertRaisesRegex(library.LibraryError, "no retry"):
                cache._run([sys.executable, "-c", "raise SystemExit(17)"],
                           b"", 8192, time.monotonic() + 3)
        spawn.assert_called_once()


if __name__ == "__main__":
    unittest.main()
