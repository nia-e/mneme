import hashlib
import json
import os
import shutil
from pathlib import Path
import subprocess
import sys
import tempfile
import unittest
from unittest.mock import patch

sys.path.insert(0, str(Path(__file__).parent))
import library


def write(path, value):
    path.parent.mkdir(parents=True, exist_ok=True)
    path.write_text(json.dumps(value))


class LibraryTests(unittest.TestCase):
    def setUp(self):
        self.tmp = tempfile.TemporaryDirectory()
        self.addCleanup(self.tmp.cleanup)
        self.root = Path(self.tmp.name)
        self.owner = self.root / "owner"
        self.replica = self.root / "replica"
        self.inbox = self.root / "inbox"
        for base, device, peer in ((self.owner, "a", "b"), (self.replica, "b", "a")):
            if library.host_files.WINDOWS:
                library.host_files.ensure_private_directory(base)
            else:
                # Retain the historical Unix fixture's ordinary 0755 directory.
                base.mkdir()
            write(base / "library.json", {"schema": library.CONFIG_SCHEMA,
                   "library_id": "library", "device_id": device,
                   "catalog_path": "catalog.json", "owners": {}, "replicas": {}})
            write(base / "library.json.publisher.json", {"schema": library.PUBLISHER_SCHEMA,
                   "peers": [{"device_id": peer, "transport": "local_directory",
                              "inbox": str(self.inbox)}], "sources": {}, "outbox": []})
        self.project = self.root / "project"
        self.project.mkdir()
        self.owner_config = self.owner / "library.json"
        self.replica_config = self.replica / "library.json"

    def _enroll(self):
        self.assertTrue(library.enroll_project(self.owner_config, self.project, "DB1"))
        return library.status(self.owner_config)["entries"][0]

    def _register(self, config=None, root=None, db_id="DB1", **kwargs):
        return library.register_local_project(config or self.owner_config,
                root or self.project, db_id, database=kwargs.pop("database", "project"),
                owner_endpoint=kwargs.pop("owner_endpoint", "http://127.0.0.1:18001"), **kwargs)

    def test_local_registration_without_publisher_is_peerless_and_idempotent(self):
        sidecar = library._publisher_path(self.owner_config)
        sidecar.unlink()
        self.assertTrue(self._register())
        config_before = self.owner_config.read_bytes()
        catalog_before = (self.owner / "catalog.json").read_bytes()
        self.assertTrue(self._register())
        self.assertEqual(self.owner_config.read_bytes(), config_before)
        self.assertEqual((self.owner / "catalog.json").read_bytes(), catalog_before)
        config = json.loads(config_before)
        self.assertEqual(config["owner_routes"]["a"]["DB1"], {"url": "http://127.0.0.1:18001"})
        self.assertEqual(library.status(self.owner_config)["pending"], 0)
        self.assertEqual(library.status(self.owner_config)["entries"][0]["delivery"], "local")
        self.assertFalse(sidecar.exists())

    def test_local_registration_never_reads_or_writes_configured_publisher(self):
        sidecar = library._publisher_path(self.owner_config)
        before = sidecar.read_bytes()
        with patch.object(library, "_queue", side_effect=AssertionError("must not announce")):
            self.assertTrue(self._register())
            self.assertTrue(self._register(owner_endpoint="http://127.0.0.1:18002"))
        self.assertEqual(sidecar.read_bytes(), before)
        self.assertEqual(library.status(self.owner_config)["pending"], 0)
        self.assertEqual(json.loads(sidecar.read_text())["sources"], {})
        # Even malformed publication-only state does not block local setup.
        sidecar.write_text("not publisher JSON")
        self.assertTrue(self._register())
        self.assertEqual(sidecar.read_text(), "not publisher JSON")

    @unittest.skipIf(library.host_files.WINDOWS, "snapshot relay requires Unix directory durability")
    def test_publisher_timer_and_snapshot_require_explicit_enrollment_of_local_entry(self):
        self.assertTrue(self._register())
        entry = library.status(self.owner_config)["entries"][0]
        with patch.object(library, "_snapshot", side_effect=AssertionError("must not snapshot")):
            result = library.once(self.owner_config)
        self.assertEqual(set(result), {"delivery", "receive"})
        publisher = json.loads(library._publisher_path(self.owner_config).read_text())
        self.assertEqual(publisher["sources"], {})
        self.assertEqual(publisher["outbox"], [])
        with self.assertRaisesRegex(library.LibraryError, "not enrolled"):
            library._snapshot(self.owner_config, entry["project_id"])
        with self.assertRaisesRegex(library.LibraryError, "not enrolled"):
            library.publish(self.owner_config, entry["project_id"], self._bundle())

    def test_local_registration_creates_only_minimal_native_metadata(self):
        config = self.root / "new-library" / "library.json"
        self.assertFalse(self._register(config=config, create_config=False))
        self.assertFalse(config.parent.exists())
        self.assertTrue(self._register(config=config))
        data = json.loads(config.read_text())
        self.assertEqual(data["schema"], library.CONFIG_SCHEMA)
        self.assertNotIn("core", data)
        self.assertEqual(data["owners"], {})
        self.assertEqual(data["replicas"], {})
        self.assertFalse(library._publisher_path(config).exists())
        identity = (data["device_id"], data["library_id"])
        self.assertTrue(self._register(config=config))
        data = json.loads(config.read_text())
        self.assertEqual((data["device_id"], data["library_id"]), identity)
        self.assertEqual(library.status(config)["entries"][0]["replicas"], [])

    def test_local_registration_failures_and_withdrawal_do_not_change_metadata(self):
        self.assertTrue(self._register())
        paths = [self.owner_config, self.owner / "catalog.json", library._publisher_path(self.owner_config)]
        before = [p.read_bytes() for p in paths]
        for kwargs in ({"db_id": "DB2"}, {"database": "different"}):
            with self.assertRaisesRegex(library.LibraryError, "conflicts"):
                self._register(**kwargs)
            self.assertEqual([p.read_bytes() for p in paths], before)
        entry = library.status(self.owner_config)["entries"][0]
        publisher_before = library._publisher_path(self.owner_config).read_bytes()
        library.withdraw(self.owner_config, entry["project_id"])
        self.assertEqual(library._publisher_path(self.owner_config).read_bytes(), publisher_before)
        before = [p.read_bytes() for p in paths]
        self.assertFalse(self._register())
        self.assertEqual([p.read_bytes() for p in paths], before)

    def test_local_registration_refuses_exclusions_and_invalid_admission_without_config(self):
        config = self.root / "absent-library" / "library.json"
        for mode in ("private", "isolated"):
            write(self.project / ".mneme" / "profile.json", {"schema": "mneme.profile.v1", "mode": mode})
            self.assertFalse(self._register(config=config))
            self.assertFalse(config.parent.exists())
        write(self.project / ".mneme" / "profile.json", {"schema": "mneme.profile.v1", "mode": "oops"})
        with self.assertRaisesRegex(library.LibraryError, "profile"):
            self._register(config=config)
        (self.project / ".mneme" / "profile.json").unlink()
        for kwargs in ({"db_id": ""}, {"db_id": None}, {"database": "../bad"},
                       {"owner_endpoint": "http://192.0.2.1:18001"},
                       {"owner_endpoint": "http://127.0.0.1:bad"}, {"create_config": "true"}):
            with self.assertRaises(library.LibraryError):
                self._register(config=config, **kwargs)
            self.assertFalse(config.parent.exists())

    def test_local_registration_rejects_symlink_and_orphaned_metadata(self):
        config = self.root / "absent-library" / "library.json"
        alias = self.root / "project-alias"
        try:
            alias.symlink_to(self.project, target_is_directory=True)
        except OSError as error:
            if library.host_files.WINDOWS and getattr(error, "winerror", None) == 1314:
                self.skipTest("Windows symlink creation requires Developer Mode or privilege")
            raise
        with self.assertRaisesRegex(library.LibraryError, "real directory"):
            self._register(config=config, root=alias)
        self.assertFalse(config.parent.exists())
        if library.host_files.WINDOWS:
            library.host_files.ensure_private_directory(config.parent)
        else:
            config.parent.mkdir()
        write(config.parent / "catalog.json", {"preserved": "evidence"})
        before = (config.parent / "catalog.json").read_bytes()
        with self.assertRaisesRegex(library.LibraryError, "orphaned"):
            self._register(config=config)
        self.assertFalse(config.exists())
        self.assertEqual((config.parent / "catalog.json").read_bytes(), before)
        alias = self.root / "config-alias.json"
        alias.symlink_to(self.owner_config)
        with self.assertRaisesRegex(library.LibraryError, "symlink"):
            self._register(config=alias)
        write(self.root / "profile.json", {"schema": "mneme.profile.v1", "mode": "default"})
        (self.project / ".mneme").mkdir()
        (self.project / ".mneme" / "profile.json").symlink_to(self.root / "profile.json")
        with self.assertRaisesRegex(library.LibraryError, "symlink"):
            self._register()

    def test_local_registration_rejects_malformed_existing_config_and_catalog(self):
        config_bytes = self.owner_config.read_bytes()
        config = json.loads(config_bytes)
        config["owners"] = []
        write(self.owner_config, config)
        with self.assertRaisesRegex(library.LibraryError, "owners"):
            self._register()
        self.assertFalse((self.owner / "catalog.json").exists())
        self.owner_config.write_bytes(config_bytes)
        write(self.owner / "catalog.json", {"schema": library.SCHEMA, "library_id": "library",
              "revision": 0, "entries": [False]})
        before = (self.owner / "catalog.json").read_bytes()
        with self.assertRaisesRegex(library.LibraryError, "descriptor"):
            self._register()
        self.assertEqual((self.owner / "catalog.json").read_bytes(), before)
        self.assertNotIn("owner_routes", json.loads(self.owner_config.read_text()))

    def test_local_registration_catalog_write_retry_retains_config_identity(self):
        config = self.root / "new-library" / "library.json"
        original_write = library._write_json

        def fail_catalog(path, value):
            if Path(path).name == "catalog.json":
                raise OSError("catalog write failed")
            return original_write(path, value)

        with patch.object(library, "_write_json", side_effect=fail_catalog):
            with self.assertRaisesRegex(OSError, "catalog write failed"):
                self._register(config=config)
        data = json.loads(config.read_text())
        self.assertFalse((config.parent / "catalog.json").exists())
        self.assertTrue(self._register(config=config))
        self.assertEqual(json.loads(config.read_text())["device_id"], data["device_id"])
        self.assertFalse(library._publisher_path(config).exists())

    def test_local_registration_before_after_atomic_io_cuts_retry_exactly(self):
        boundaries = [(library, "_write_json"), (library.os, "fsync"),
                      (library.host_files, "_replace_synced_file")]
        if not library.host_files.WINDOWS:
            boundaries.append((library.host_files, "sync_directory"))
        for owner, name in boundaries:
            original = getattr(owner, name)
            count = [0]

            def record(*args, **kwargs):
                count[0] += 1
                return original(*args, **kwargs)

            probe = self.root / f"probe-{name}" / "library.json"
            with patch.object(owner, name, side_effect=record):
                self.assertTrue(self._register(config=probe))
            self.assertGreater(count[0], 0, f"unexercised publication boundary: {name}")
            for cut in range(1, count[0] + 1):
                for after in (False, True):
                    with self.subTest(operation=name, cut=cut, after=after):
                        config = self.root / f"cut-{name}-{cut}-{after}" / "library.json"
                        calls = [0]

                        def fail(*args, **kwargs):
                            calls[0] += 1
                            current = calls[0]
                            if current == cut and not after:
                                raise OSError("injected atomic IO failure")
                            result = original(*args, **kwargs)
                            if current == cut and after:
                                raise OSError("injected atomic IO failure")
                            return result

                        with patch.object(owner, name, side_effect=fail):
                            with self.assertRaisesRegex(OSError, "injected atomic IO failure"):
                                self._register(config=config)
                        identity = json.loads(config.read_text())["device_id"] if config.exists() else None
                        self.assertTrue(self._register(config=config))
                        data = json.loads(config.read_text())
                        if identity is not None:
                            self.assertEqual(data["device_id"], identity)
                        entries = library.status(config)["entries"]
                        self.assertEqual(len(entries), 1)
                        self.assertEqual(entries[0]["db_id"], "DB1")
                        self.assertEqual(entries[0]["delivery"], "local")
                        self.assertFalse(library._publisher_path(config).exists())
                        self.assertEqual(list(config.parent.glob(".mneme-publish-*")), [])

    def test_concurrent_local_registration_serializes_one_config_identity(self):
        config = self.root / "new-library" / "library.json"
        second = self.root / "project2"
        second.mkdir()
        code = ("import sys; from pathlib import Path; "
                "sys.path.insert(0,sys.argv[1]); import library; sys.stdin.readline(); "
                "print(library.register_local_project(Path(sys.argv[2]),Path(sys.argv[3]),sys.argv[4],"
                "database='project',owner_endpoint='http://127.0.0.1:18001'))")
        children = [subprocess.Popen([sys.executable, "-c", code, str(Path(library.__file__).parent),
                    str(config), str(project), db_id], stdin=subprocess.PIPE,
                    stdout=subprocess.PIPE, stderr=subprocess.PIPE)
                    for project, db_id in ((self.project, "DB1"), (second, "DB2"))]
        for child in children:
            child.stdin.write(b"start\n")
            child.stdin.flush()
        for child in children:
            stdout, stderr = child.communicate(timeout=15)
            self.assertEqual(child.returncode, 0, stderr.decode())
            self.assertEqual(stdout.strip(), b"True")
        data = json.loads(config.read_text())
        entries = library.status(config)["entries"]
        self.assertEqual({e["owner_device_id"] for e in entries}, {data["device_id"]})
        self.assertEqual({e["db_id"] for e in entries}, {"DB1", "DB2"})
        self.assertEqual(set(data["owner_routes"][data["device_id"]]), {"DB1", "DB2"})
        self.assertFalse(library._publisher_path(config).exists())

    @unittest.skipIf(library.host_files.WINDOWS, "Unix retained-directory mode contract")
    def test_existing_unix_0755_state_directory_is_accepted_without_chmod(self):
        retained = self.root / "retained-state"
        retained.mkdir(mode=0o755)
        retained.chmod(0o755)
        evidence = retained / "unrelated-evidence"
        evidence.write_bytes(b"keep these bytes and permissions")
        evidence.chmod(0o644)
        before = (evidence.read_bytes(), evidence.stat().st_mode, evidence.stat().st_mtime_ns)
        config = retained / "library.json"
        self.assertTrue(self._register(config=config))
        self.assertEqual(retained.stat().st_mode & 0o777, 0o755)
        self.assertEqual((evidence.read_bytes(), evidence.stat().st_mode, evidence.stat().st_mtime_ns), before)
        for path in (config, retained / "catalog.json", Path(str(config) + ".lock")):
            library.host_files.check_private(path)
        self.assertEqual(library.status(config)["entries"][0]["delivery"], "local")
        self.assertEqual(retained.stat().st_mode & 0o777, 0o755)

    def test_created_local_state_is_private_and_status_does_not_mutate(self):
        config = self.root / "new-library" / "nested" / "library.json"
        self.assertTrue(self._register(config=config))
        for directory in (config.parent, config.parent.parent):
            library.host_files.check_private_directory(directory)
        paths = (config, config.parent / "catalog.json", Path(str(config) + ".lock"))
        for path in paths:
            library.host_files.check_private(path)
        before = {path: (path.read_bytes(), path.stat().st_mtime_ns) for path in paths}
        self.assertEqual(library.status(config)["entries"][0]["delivery"], "local")
        self.assertEqual({path: (path.read_bytes(), path.stat().st_mtime_ns) for path in paths}, before)

    def test_metadata_and_lock_reject_hardlinks_without_modifying_evidence(self):
        self.assertTrue(self._register())
        alias = self.owner / "linked.json"
        os.link(self.owner_config, alias)
        before = self.owner_config.read_bytes()
        with self.assertRaises(library.LibraryError):
            library._read_json(alias)
        with self.assertRaises(library.LibraryError):
            library._write_json(alias, {"unexpected": "replacement"})
        self.assertEqual(self.owner_config.read_bytes(), before)
        lock = self.root / "lock-config.lock"
        lock.write_bytes(b"exact prior lock evidence")
        os.link(lock, self.root / "lock-alias")
        with self.assertRaises(library.LibraryError):
            with library._lock(self.root / "lock-config"):
                self.fail("multiply-linked lock was admitted")
        self.assertEqual(lock.read_bytes(), b"exact prior lock evidence")

    def test_windows_relay_refuses_before_any_state_or_provider_work(self):
        missing = self.root / "absent" / "library.json"
        operations = (
            lambda: library.enroll_project(missing, self.project, "DB1"),
            lambda: library.publish(missing, "project", self.root / "bundle"),
            lambda: library.sync(missing),
            lambda: library.once(missing),
            lambda: library._snapshot(missing, "project"),
            lambda: library.receive(missing, b"", authenticated_peer="owner"),
            lambda: library.receive_inbox(missing),
            lambda: library.serve(missing, self.root / "mcp", "127.0.0.1:8080"),
        )
        with patch.object(library.host_files, "WINDOWS", True), \
                patch.object(library, "_lock", side_effect=AssertionError("lock before denial")), \
                patch.object(library, "_load", side_effect=AssertionError("state before denial")), \
                patch.object(library.subprocess, "run", side_effect=AssertionError("provider before denial")):
            for operation in operations:
                with self.assertRaisesRegex(library.LibraryError, "snapshot relay is unsupported on Windows"):
                    operation()
        self.assertFalse(missing.parent.exists())

    def test_standalone_installed_helper_finds_packaged_file_primitives(self):
        install = self.root / "installed" / "lib"
        install.mkdir(parents=True)
        for path in (Path(library.__file__), Path(library.host_files.__file__)):
            shutil.copyfile(path, install / path.name)
        env = dict(os.environ)
        env.pop("PYTHONPATH", None)
        result = subprocess.run([sys.executable, str(install / "library.py"), "--help"],
                                cwd=self.root, env=env, capture_output=True, timeout=15)
        self.assertEqual(result.returncode, 0, result.stderr.decode())
        self.assertIn(b"status", result.stdout)

    @unittest.skipIf(library.host_files.WINDOWS, "snapshot relay requires Unix directory durability")
    def test_explicit_enrollment_promotes_local_descriptor_and_can_restore_withdrawal(self):
        self.assertTrue(self._register())
        entry = library.status(self.owner_config)["entries"][0]
        self.assertTrue(library.enroll_project(self.owner_config, self.project, "DB1"))
        publisher = json.loads(library._publisher_path(self.owner_config).read_text())
        self.assertEqual(publisher["sources"][entry["project_id"]]["database"], "project")
        self.assertEqual(len(publisher["outbox"]), 1)
        self.assertTrue(library.enroll_project(self.owner_config, self.project, "DB1"))
        self.assertEqual(json.loads(library._publisher_path(self.owner_config).read_text()), publisher)
        library.withdraw(self.owner_config, entry["project_id"])
        self.assertFalse(library.enroll_project(self.owner_config, self.project, "DB1"))
        self.assertTrue(library.enroll_project(self.owner_config, self.project, "DB1", allow_reenroll=True))
        restored = library.status(self.owner_config)["entries"][0]
        self.assertFalse(restored["withdrawn"])
        self.assertEqual(restored["revision"], 3)

    def _deliver(self):
        self.assertEqual(library.sync(self.owner_config)["pending"], 1)
        files = list(self.inbox.glob("*.tar"))
        self.assertEqual(len(files), 1)
        data = files[0].read_bytes()
        response = library.receive(self.replica_config, data, authenticated_peer="a", include_ack=True)
        write(files[0].with_suffix(".ack.json"), response)
        files[0].unlink()
        self.assertEqual(library.sync(self.owner_config)["pending"], 0)
        return response["result"]

    def _bundle(self, generation="GEN1", captured_at=100, db_id="DB1"):
        bundle = self.root / generation
        bundle.mkdir()
        database = b"database bytes"
        body = b"body bytes"
        (bundle / "database.db").write_bytes(database)
        (bundle / "bodies").mkdir()
        (bundle / "bodies" / "body1").write_bytes(body)
        files = []
        for path, payload in (("database.db", database), ("bodies/body1", body)):
            files.append({"path": path, "size": len(payload),
                          "sha256": hashlib.sha256(payload).hexdigest()})
        write(bundle / "manifest.json", {"schema": library.SNAPSHOT_SCHEMA,
              "db_id": db_id, "generation": generation, "captured_at": captured_at,
              "embedding_fingerprint": {}, "files": files})
        return bundle

    @unittest.skipIf(library.host_files.WINDOWS, "snapshot relay requires Unix directory durability")
    def test_archive_preserves_prior_member_bytes_for_pending_delivery_retry(self):
        bundle = self._bundle()
        descriptor = {"db_id": "DB1"}
        delivery = "b" * 32
        message = library._message(descriptor, bundle, delivery)
        # The pre-port writer used TarFile.add's filesystem metadata. Changing
        # it would conflict with a queued delivery's already-published .tar.
        previous = library.io.BytesIO()
        with library.tarfile.open(fileobj=previous, mode="w") as archive:
            data = json.dumps(message, sort_keys=True, separators=(",", ":")).encode()
            info = library.tarfile.TarInfo("message.json")
            info.size = len(data)
            archive.addfile(info, library.io.BytesIO(data))
            names = ["manifest.json"] + [item["path"] for item in message["snapshot"]["files"]]
            for name in names:
                archive.add(bundle / name, arcname="snapshot/" + name, recursive=False)
        self.assertEqual(library._make_archive(descriptor, bundle, delivery), previous.getvalue())

    @unittest.skipIf(library.host_files.WINDOWS, "snapshot relay requires Unix directory durability")
    def test_descriptor_arrives_before_snapshot(self):
        entry = self._enroll()
        self.assertEqual(library.status(self.owner_config)["pending"], 1)
        self.assertEqual(self._deliver(), "announced")
        received = library.status(self.replica_config)["entries"][0]
        self.assertEqual(received["db_id"], "DB1")
        self.assertEqual(received["replicas"], [])

    @unittest.skipIf(library.host_files.WINDOWS, "snapshot relay requires Unix directory durability")
    def test_offline_send_keeps_pending_and_retries(self):
        self._enroll()
        settings = json.loads(Path(str(self.owner_config) + ".publisher.json").read_text())
        settings["peers"][0]["inbox"] = str(self.root / "not-a-dir")
        (self.root / "not-a-dir").write_text("blocked")
        write(Path(str(self.owner_config) + ".publisher.json"), settings)
        self.assertEqual(library.sync(self.owner_config)["pending"], 1)
        self.assertEqual(library.status(self.owner_config)["entries"][0]["delivery"], "pending")
        settings["peers"][0]["inbox"] = str(self.inbox)
        write(Path(str(self.owner_config) + ".publisher.json"), settings)
        self.assertEqual(self._deliver(), "announced")

    @unittest.skipIf(library.host_files.WINDOWS, "snapshot relay requires Unix directory durability")
    def test_replay_stale_withdrawal_and_identity_conflict(self):
        entry = self._enroll()
        data = library._make_archive(entry, None)
        self.assertEqual(library.receive(self.replica_config, data, authenticated_peer="a"), "announced")
        self.assertEqual(library.receive(self.replica_config, data, authenticated_peer="a"), "replay")
        withdrawn = library.withdraw(self.owner_config, entry["project_id"])
        self.assertEqual(library.receive(self.replica_config,
                         library._make_archive(withdrawn, None), authenticated_peer="a"), "announced")
        self.assertEqual(library.receive(self.replica_config, data, authenticated_peer="a"), "stale")
        self.assertTrue(library.status(self.replica_config)["entries"][0]["withdrawn"])
        conflict = dict(withdrawn, db_id="DB2", revision=3)
        with self.assertRaisesRegex(library.LibraryError, "identity conflict"):
            library.receive(self.replica_config, library._make_archive(conflict, None),
                            authenticated_peer="a")

    @unittest.skipIf(library.host_files.WINDOWS, "snapshot relay requires Unix directory durability")
    def test_partial_and_corrupt_bundle_cannot_activate(self):
        entry = self._enroll()
        self.assertEqual(self._deliver(), "announced")
        bundle = self._bundle()
        (bundle / "bodies" / "body1").write_bytes(b"oops")
        with self.assertRaises(library.LibraryError):
            library.publish(self.owner_config, entry["project_id"], bundle)
        self.assertEqual(library.status(self.replica_config)["entries"][0]["replicas"], [])
        (bundle / "bodies" / "body1").unlink()
        with self.assertRaises(library.LibraryError):
            library.validate_bundle(bundle, "DB1")

    @unittest.skipIf(library.host_files.WINDOWS, "snapshot relay requires Unix directory durability")
    def test_private_and_isolated_never_enroll(self):
        for mode in ("private", "isolated"):
            write(self.project / ".mneme" / "profile.json", {"schema": "mneme.profile.v1", "mode": mode})
            self.assertFalse(library.enroll_project(self.owner_config, self.project, "DB1",
                             owner_endpoint="http://127.0.0.1:18001"))
            self.assertEqual(library.status(self.owner_config)["entries"], [])
            self.assertNotIn("owner_routes", json.loads(self.owner_config.read_text()))

    @unittest.skipIf(library.host_files.WINDOWS, "snapshot relay requires Unix directory durability")
    def test_private_transition_supersedes_pending_and_withdrawal_sticks(self):
        entry = self._enroll()
        write(self.project / ".mneme" / "profile.json", {"schema": "mneme.profile.v1", "mode": "private"})
        library.sync(self.owner_config)
        current = library.status(self.owner_config)["entries"][0]
        self.assertTrue(current["withdrawn"])
        self.assertEqual(current["revision"], 2)
        self.assertEqual(library.receive(self.replica_config,
                         library._make_archive(current, None), authenticated_peer="a"), "announced")
        (self.project / ".mneme" / "profile.json").unlink()
        self.assertFalse(library.enroll_project(self.owner_config, self.project, "DB1"))
        with self.assertRaisesRegex(library.LibraryError, "withdrawn"):
            library.publish(self.owner_config, entry["project_id"], self._bundle())

    @unittest.skipIf(library.host_files.WINDOWS, "snapshot relay requires Unix directory durability")
    def test_snapshot_activation_keeps_old_generation_file(self):
        entry = self._enroll()
        self._deliver()
        one = self._bundle("GEN1")
        data1 = library._make_archive(entry, one)
        self.assertEqual(library.receive(self.replica_config, data1, authenticated_peer="a"), "activated")
        old_database = Path(library.status(self.replica_config)["entries"][0]["replicas"][0]["resolved_path"])
        self.assertTrue(old_database.with_suffix(".bodies").joinpath("body1").is_file())
        with old_database.open("rb") as open_old:
            two = self._bundle("GEN2", 101)
            data2 = library._make_archive(entry, two)
            self.assertEqual(library.receive(self.replica_config, data2, authenticated_peer="a"), "activated")
            self.assertEqual(open_old.read(), b"database bytes")
        replicas = library.status(self.replica_config)["entries"][0]["replicas"]
        self.assertEqual([r["generation"] for r in replicas], ["GEN2", "GEN1"])
        self.assertTrue(old_database.exists())
        self.assertNotIn("/", replicas[0]["database"])
        self.assertNotEqual(replicas[0]["database"], replicas[1]["database"])
        registry = library.serving_registry(self.replica_config)
        self.assertEqual(set(registry), {r["database"] for r in replicas})
        self.assertNotEqual(registry[replicas[0]["database"]], registry[replicas[1]["database"]])
        self.assertEqual(registry[replicas[1]["database"]], old_database)
        unknown = old_database.parent.parent.parent / "unknown-evidence"
        unknown.mkdir()
        three = self._bundle("GEN3", 102)
        self.assertEqual(library.receive(self.replica_config,
                         library._make_archive(entry, three), authenticated_peer="a"), "activated")
        self.assertFalse(old_database.exists())
        self.assertTrue(unknown.exists())

    @unittest.skipIf(library.host_files.WINDOWS, "snapshot relay requires Unix directory durability")
    def test_same_second_newer_generation_wins_older_pair_is_stale(self):
        entry = self._enroll()
        self._deliver()
        first = self._bundle("GENA", 100)
        second = self._bundle("GENB", 100)
        a = library._make_archive(entry, first)
        b = library._make_archive(entry, second)
        self.assertEqual(library.receive(self.replica_config, a, authenticated_peer="a"), "activated")
        self.assertEqual(library.receive(self.replica_config, b, authenticated_peer="a"), "activated")
        self.assertEqual(library.receive(self.replica_config, a, authenticated_peer="a"), "stale")
        self.assertEqual(library.status(self.replica_config)["entries"][0]["replicas"][0]["generation"], "GENB")

    @unittest.skipIf(library.host_files.WINDOWS, "snapshot relay requires Unix directory durability")
    def test_ssh_uses_fixed_user_local_receiver_command(self):
        peer = {"device_id": "b", "transport": "ssh", "host": "user@pi", "port": 2222}
        completed = subprocess.CompletedProcess([], 0, stdout=b"{}")
        with patch.object(library.subprocess, "run", return_value=completed) as run:
            library._peer_send(peer, b"archive", "a" * 32)
        argv = run.call_args.args[0]
        self.assertEqual(argv[-2:], ["user@pi", "exec ~/.local/bin/mneme-library-receive"])
        self.assertEqual(run.call_args.kwargs["input"], b"archive")

    @unittest.skipIf(library.host_files.WINDOWS, "snapshot relay requires Unix directory durability")
    def test_replica_service_stops_before_catalog_switch(self):
        entry = self._enroll()
        self._deliver()
        settings_path = Path(str(self.replica_config) + ".publisher.json")
        settings = json.loads(settings_path.read_text())
        settings["serving"] = {"stop": ["/bin/true"], "start": ["/bin/true"]}
        write(settings_path, settings)
        observed = []

        def command(publisher, action):
            catalog = library.status(self.replica_config)["entries"][0]
            observed.append((action, len(catalog["replicas"])))

        with patch.object(library, "_service_command", side_effect=command):
            result = library.receive(self.replica_config,
                        library._make_archive(entry, self._bundle()), authenticated_peer="a")
        self.assertEqual(result, "activated")
        self.assertEqual(observed, [("stop", 0), ("start", 1)])

    @unittest.skipIf(library.host_files.WINDOWS, "snapshot relay requires Unix directory durability")
    def test_hostile_project_path_and_failed_stop_never_switch(self):
        entry = self._enroll()
        self._deliver()
        hostile = dict(entry, project_id="../escape")
        with self.assertRaisesRegex(library.LibraryError, "project_id"):
            library.receive(self.replica_config, library._make_archive(hostile, None),
                            authenticated_peer="a")
        with patch.object(library, "_service_command", side_effect=library.LibraryError("stop failed")):
            with self.assertRaisesRegex(library.LibraryError, "stop failed"):
                library.receive(self.replica_config,
                    library._make_archive(entry, self._bundle()), authenticated_peer="a")
        self.assertEqual(library.status(self.replica_config)["entries"][0]["replicas"], [])

    @unittest.skipIf(library.host_files.WINDOWS, "snapshot relay requires Unix directory durability")
    def test_native_source_prune_keeps_two_pending_and_unknown(self):
        entry = self._enroll()
        bundles = [self._bundle(f"GEN{i}", 100 + i) for i in range(1, 4)]
        for bundle in bundles:
            manifest = library.validate_bundle(bundle, "DB1")
            library._record_native_bundle(self.owner_config, entry["project_id"], bundle, manifest)
        unknown = self.root / "unknown-generation"
        unknown.mkdir()
        other_db = self.root / "other-db-generation"
        other_db.mkdir()
        sidecar = Path(str(self.owner_config) + ".publisher.json")
        data = json.loads(sidecar.read_text())
        # Simulate a durable failed delivery still referencing oldest bytes.
        data["outbox"] = [{"peer_device_id": "b", "descriptor": entry,
                            "bundle": str(bundles[0])}]
        write(sidecar, data)
        self.assertEqual(library._prune_native_bundles(data), 0)
        self.assertTrue(all(bundle.exists() for bundle in bundles))
        data["outbox"] = []
        self.assertEqual(library._prune_native_bundles(data), 1)
        self.assertFalse(bundles[0].exists())
        self.assertTrue(bundles[1].exists() and bundles[2].exists())
        self.assertTrue(unknown.exists() and other_db.exists())

    @unittest.skipIf(library.host_files.WINDOWS, "snapshot relay requires Unix directory durability")
    def test_snapshot_create_response_records_only_verified_bundle(self):
        entry = self._enroll()
        bundle = self._bundle("GEN1")
        sidecar = Path(str(self.owner_config) + ".publisher.json")
        settings = json.loads(sidecar.read_text())
        settings["snapshot"] = {"mnemed": "/bin/true", "remote_url": "http://127.0.0.1:18766"}
        write(sidecar, settings)
        response = subprocess.CompletedProcess([], 0,
            stdout=json.dumps({"bundle": str(bundle), "db_id": "DB1", "generation": "GEN1"}).encode())
        with patch.object(library.subprocess, "run", return_value=response) as run:
            self.assertEqual(library._snapshot(self.owner_config, entry["project_id"]), bundle)
        self.assertEqual(run.call_args.args[0][-3:], ["project", "snapshot", "create"])
        records = json.loads(sidecar.read_text())["sources"][entry["project_id"]]["native_bundles"]
        self.assertEqual(records[0]["path"], str(bundle.resolve()))

    @unittest.skipIf(library.host_files.WINDOWS, "snapshot relay requires Unix directory durability")
    def test_two_owner_projects_route_snapshot_to_distinct_local_ports(self):
        self.assertTrue(library.enroll_project(self.owner_config, self.project, "DB1",
                        owner_endpoint={"url": "http://127.0.0.1:18001", "token_env": "OWNER_TOKEN"}))
        second = self.root / "project2"
        second.mkdir()
        self.assertTrue(library.enroll_project(self.owner_config, second, "DB2",
                        owner_endpoint="http://127.0.0.1:18002"))
        entries = {e["db_id"]: e for e in library.status(self.owner_config)["entries"]}
        config = json.loads(self.owner_config.read_text())
        self.assertEqual(config["owner_routes"]["a"]["DB1"]["url"], "http://127.0.0.1:18001")
        self.assertEqual(config["owner_routes"]["a"]["DB1"]["token_env"], "OWNER_TOKEN")
        self.assertEqual(config["owner_routes"]["a"]["DB2"]["url"], "http://127.0.0.1:18002")
        self.assertNotIn("owner_routes", json.loads(Path(str(self.owner_config) + ".publisher.json").read_text()))
        settings_path = Path(str(self.owner_config) + ".publisher.json")
        settings = json.loads(settings_path.read_text())
        settings["snapshot"] = {"mnemed": "/bin/true", "remote_url": "http://127.0.0.1:19999"}
        write(settings_path, settings)
        one = self._bundle("GEN1", db_id="DB1")
        two = self._bundle("GEN2", db_id="DB2")
        responses = [subprocess.CompletedProcess([], 0, stdout=json.dumps({
            "bundle": str(bundle), "db_id": db_id, "generation": generation}).encode())
            for bundle, db_id, generation in ((one, "DB1", "GEN1"), (two, "DB2", "GEN2"))]
        with patch.object(library.subprocess, "run", side_effect=responses) as run:
            library._snapshot(self.owner_config, entries["DB1"]["project_id"])
            library._snapshot(self.owner_config, entries["DB2"]["project_id"])
        commands = [call.args[0] for call in run.call_args_list]
        self.assertEqual(commands[0][commands[0].index("--remote") + 1], "http://127.0.0.1:18001")
        self.assertEqual(commands[0][commands[0].index("--remote-token-env") + 1], "OWNER_TOKEN")
        self.assertEqual(commands[1][commands[1].index("--remote") + 1], "http://127.0.0.1:18002")

    @unittest.skipIf(library.host_files.WINDOWS, "snapshot relay requires Unix directory durability")
    def test_sender_catalog_learns_receiver_current_and_previous(self):
        entry = self._enroll()
        self._deliver()
        settings_path = Path(str(self.replica_config) + ".publisher.json")
        settings = json.loads(settings_path.read_text())
        settings["peers"][0]["receive_inbox"] = str(self.inbox)
        write(settings_path, settings)
        for generation, captured in (("GEN1", 100), ("GEN2", 101)):
            result = library.publish(self.owner_config, entry["project_id"],
                                     self._bundle(generation, captured))
            self.assertEqual(result["pending"], 1)
            self.assertEqual(library.receive_inbox(self.replica_config)["accepted"], 1)
            self.assertEqual(library.sync(self.owner_config)["pending"], 0)
        owner_replicas = library.status(self.owner_config)["entries"][0]["replicas"]
        self.assertEqual([r["generation"] for r in owner_replicas], ["GEN2", "GEN1"])
        self.assertTrue(all(r["source_device_id"] == "b" for r in owner_replicas))
        self.assertNotEqual(owner_replicas[0]["database"], owner_replicas[1]["database"])

    @unittest.skipIf(library.host_files.WINDOWS, "snapshot relay requires Unix directory durability")
    def test_wrong_identity_ack_does_not_populate_sender_catalog(self):
        entry = self._enroll()
        self._deliver()
        bundle = self._bundle()
        self.assertEqual(library.publish(self.owner_config, entry["project_id"], bundle)["pending"], 1)
        tar = next(self.inbox.glob("*.tar"))
        response = library.receive(self.replica_config, tar.read_bytes(),
                                   authenticated_peer="a", include_ack=True)
        response["ack"]["receiver_device_id"] = "not-b"
        write(tar.with_suffix(".ack.json"), response)
        self.assertEqual(library.sync(self.owner_config)["pending"], 1)
        self.assertEqual(library.status(self.owner_config)["entries"][0]["replicas"], [])

    @unittest.skipIf(library.host_files.WINDOWS, "snapshot relay requires Unix directory durability")
    def test_ssh_ack_populates_sender_catalog(self):
        entry = self._enroll()
        owner_sidecar = Path(str(self.owner_config) + ".publisher.json")
        settings = json.loads(owner_sidecar.read_text())
        settings["peers"] = [{"device_id": "b", "transport": "ssh", "host": "user@pi"}]
        write(owner_sidecar, settings)

        def remote(argv, *, input, **_kwargs):
            response = library.receive(self.replica_config, input,
                                       authenticated_peer="a", include_ack=True)
            return subprocess.CompletedProcess(argv, 0, stdout=json.dumps(response).encode())

        with patch.object(library.subprocess, "run", side_effect=remote):
            self.assertEqual(library.sync(self.owner_config)["pending"], 0)
            self.assertEqual(library.publish(self.owner_config, entry["project_id"],
                             self._bundle())["pending"], 0)
        replicas = library.status(self.owner_config)["entries"][0]["replicas"]
        self.assertEqual([r["generation"] for r in replicas], ["GEN1"])
        self.assertEqual(replicas[0]["source_device_id"], "b")


if __name__ == "__main__":
    unittest.main()
