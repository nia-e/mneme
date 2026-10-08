"""Bounded receive-publication boundary attacks, without a server or real store.

The byte fixtures exercise the helper's closed-inventory protocol, not database
recovery. os._exit cuts exercise process death, not loss of filesystem power.
"""
from contextlib import ExitStack
import hashlib
import json
import os
from pathlib import Path
import subprocess
import sys
import tempfile
import unittest
from unittest.mock import patch

sys.path.insert(0, str(Path(__file__).parent))
import library


class InjectedFailure(OSError):
    pass


def write_json(path, value):
    path.parent.mkdir(parents=True, exist_ok=True)
    path.write_text(json.dumps(value))


def bundle(root, generation, captured_at):
    path = root / generation
    path.mkdir()
    files = []
    for name in ("database.db", "bodies/body1"):
        payload = f"{generation}:{name}".encode()
        target = path / name
        target.parent.mkdir(parents=True, exist_ok=True)
        target.write_bytes(payload)
        files.append({"path": name, "size": len(payload),
                      "sha256": hashlib.sha256(payload).hexdigest()})
    write_json(path / "manifest.json", {
        "schema": library.SNAPSHOT_SCHEMA, "db_id": "DB1",
        "generation": generation, "captured_at": captured_at,
        "embedding_fingerprint": {}, "files": files})
    return path


class ReceiveFixture:
    def __init__(self, root):
        self.root = Path(root).resolve()
        self.config = self.root / "replica" / "library.json"
        self.catalog = self.config.parent / "catalog.json"
        write_json(self.config, {
            "schema": library.CONFIG_SCHEMA, "library_id": "library",
            "device_id": "replica", "catalog_path": "catalog.json",
            "owners": {}, "replicas": {}})
        write_json(library._publisher_path(self.config), {
            "schema": library.PUBLISHER_SCHEMA,
            "peers": [{"device_id": "owner"}], "sources": {}, "outbox": []})
        self.descriptor = {
            "project_id": "project", "db_id": "DB1", "owner_device_id": "owner",
            "display_name": "fixture", "database": "project", "revision": 1,
            "withdrawn": False}
        self.parent = self.config.parent / "library-replicas" / "owner" / "project"
        for i in (1, 2):
            data = library._make_archive(self.descriptor, bundle(self.root, f"GEN{i}", i))
            library.receive(self.config, data, authenticated_peer="owner")
        self.archive = library._make_archive(
            self.descriptor, bundle(self.root, "GEN3", 3), delivery_id="a" * 32)
        self.archive_path = self.root / "delivery.tar"
        self.archive_path.write_bytes(self.archive)
        self.unknown = self.parent / "unknown-evidence" / "keep.txt"
        self.unknown.parent.mkdir()
        self.unknown.write_bytes(b"not our cleanup inventory")

    def receive(self):
        return library.receive(self.config, self.archive,
                               authenticated_peer="owner", include_ack=True)


class BoundaryCuts:
    """Wrap actual I/O; fail once immediately before/after one named boundary."""
    OPERATIONS = (
        "payload_write", "payload_sync", "verify", "serving_copy",
        "serving_bodies", "prepared_sync", "generation_rename",
        "generation_parent_sync", "service_stop", "catalog_write",
        "catalog_file_sync", "catalog_replace", "catalog_parent_sync",
        "service_start", "cleanup", "ack", "stage_cleanup",
    )

    def __init__(self, config, cut, die=False):
        self.config = Path(config).resolve()
        self.catalog = self.config.parent / "catalog.json"
        self.parent = self.config.parent / "library-replicas" / "owner" / "project"
        self.cut, self.die, self.hit = cut, die, False
        self.phase = None
        self.last_extracted = None
        self.stack = ExitStack()

    def trip(self, point):
        if point != self.cut or self.hit:
            return
        self.hit = True
        if self.die:
            os._exit(73)
        raise InjectedFailure(point)

    def around(self, name, fn, *args, **kwargs):
        self.trip(name + ":before")
        value = fn(*args, **kwargs)
        self.trip(name + ":after")
        return value

    def __enter__(self):
        original_extract = library._extract
        original_copyfileobj = library.shutil.copyfileobj
        original_copy2 = library.shutil.copy2
        original_copytree = library.shutil.copytree
        original_fsync = library.os.fsync
        original_sync_dir = library._sync_dir
        original_rename = library.os.rename
        original_replace = library.os.replace
        original_write = library._write_json
        original_validate = library.validate_bundle
        original_service = library._service_command
        original_rmtree = library.shutil.rmtree
        original_result = library._receive_result
        original_open = Path.open

        def extract(*args, **kwargs):
            self.phase = "extract"
            try:
                return original_extract(*args, **kwargs)
            finally:
                self.phase = None

        def copyfileobj(source, dest, *args, **kwargs):
            self.last_extracted = str(getattr(dest, "name", ""))
            if self.phase == "extract" and self.last_extracted.endswith("snapshot/database.db"):
                return self.around("payload_write", original_copyfileobj,
                                   source, dest, *args, **kwargs)
            return original_copyfileobj(source, dest, *args, **kwargs)

        def fsync(fd):
            if self.phase == "extract" and (self.last_extracted or "").endswith("snapshot/database.db"):
                return self.around("payload_sync", original_fsync, fd)
            if self.phase == "catalog":
                return self.around("catalog_file_sync", original_fsync, fd)
            return original_fsync(fd)

        def sync_dir(path):
            path = Path(path)
            name = None
            if path.name == "prepared" and path.parent.name.startswith(".stage-"):
                name = "prepared_sync"
            elif path == self.parent:
                name = "generation_parent_sync"
            elif path == self.catalog.parent and self.phase == "catalog":
                name = "catalog_parent_sync"
            # Directory fsync is separately classified, not catalog file sync.
            phase, self.phase = self.phase, None
            try:
                return (self.around(name, original_sync_dir, path) if name
                        else original_sync_dir(path))
            finally:
                self.phase = phase

        def rename(source, dest, *args, **kwargs):
            if Path(dest) == self.parent / "GEN3":
                return self.around("generation_rename", original_rename,
                                   source, dest, *args, **kwargs)
            return original_rename(source, dest, *args, **kwargs)

        def replace(source, dest, *args, **kwargs):
            if Path(dest) == self.catalog:
                return self.around("catalog_replace", original_replace,
                                   source, dest, *args, **kwargs)
            return original_replace(source, dest, *args, **kwargs)

        def write(path, value):
            if Path(path) != self.catalog:
                return original_write(path, value)
            self.phase = "catalog"
            try:
                return original_write(path, value)
            finally:
                self.phase = None

        cuts = self

        class CatalogFile:
            def __init__(self, stream):
                self.stream = stream

            def __getattr__(self, name):
                return getattr(self.stream, name)

            def __enter__(self):
                self.stream.__enter__()
                return self

            def __exit__(self, *args):
                return self.stream.__exit__(*args)

            def write(self, data):
                return cuts.around("catalog_write", self.stream.write, data)

        def open_path(path, *args, **kwargs):
            stream = original_open(path, *args, **kwargs)
            if (path.parent == self.catalog.parent
                    and path.name.startswith(self.catalog.name + ".tmp-")):
                return CatalogFile(stream)
            return stream

        def validate(path, *args, **kwargs):
            if Path(path).name == "snapshot":
                return self.around("verify", original_validate, path, *args, **kwargs)
            return original_validate(path, *args, **kwargs)

        def copy2(source, dest, *args, **kwargs):
            if Path(dest).name == "database.db" and Path(dest).parent.name == "serve":
                return self.around("serving_copy", original_copy2,
                                   source, dest, *args, **kwargs)
            return original_copy2(source, dest, *args, **kwargs)

        def copytree(source, dest, *args, **kwargs):
            if Path(dest).name == "database.bodies":
                return self.around("serving_bodies", original_copytree,
                                   source, dest, *args, **kwargs)
            return original_copytree(source, dest, *args, **kwargs)

        def service(publisher, action):
            return self.around("service_" + action, original_service, publisher, action)

        def rmtree(path, *args, **kwargs):
            path = Path(path)
            if path == self.parent / "GEN1":
                return self.around("cleanup", original_rmtree, path, *args, **kwargs)
            if path.name.startswith(".stage-"):
                return self.around("stage_cleanup", original_rmtree, path, *args, **kwargs)
            return original_rmtree(path, *args, **kwargs)

        def result(*args, **kwargs):
            return self.around("ack", original_result, *args, **kwargs)

        for obj, name, wrapper in (
            (library, "_extract", extract),
            (library.shutil, "copyfileobj", copyfileobj),
            (library.shutil, "copy2", copy2),
            (library.shutil, "copytree", copytree),
            (library.os, "fsync", fsync), (library, "_sync_dir", sync_dir),
            (library.os, "rename", rename), (library.os, "replace", replace),
            (library, "_write_json", write), (Path, "open", open_path),
            (library, "validate_bundle", validate),
            (library, "_service_command", service),
            (library.shutil, "rmtree", rmtree),
            (library, "_receive_result", result),
        ):
            self.stack.enter_context(patch.object(obj, name, wrapper))
        return self

    def __exit__(self, *args):
        return self.stack.__exit__(*args)


class AckFixture(ReceiveFixture):
    def __init__(self, root):
        super().__init__(root)
        self.owner = self.root / "owner" / "library.json"
        self.inbox = self.root / "inbox"
        self.inbox.mkdir()
        write_json(self.owner, {
            "schema": library.CONFIG_SCHEMA, "library_id": "library",
            "device_id": "owner", "catalog_path": "catalog.json",
            "owners": {}, "replicas": {}})
        entry = dict(self.descriptor, replicas=library.status(self.config)["entries"][0]["replicas"])
        write_json(self.owner.parent / "catalog.json", {
            "schema": library.SCHEMA, "library_id": "library", "revision": 1,
            "entries": [entry]})
        for i in (4, 5):
            bundle(self.root, f"GEN{i}", i)
        write_json(library._publisher_path(self.owner), {
            "schema": library.PUBLISHER_SCHEMA,
            "peers": [{"device_id": "replica", "transport": "local_directory",
                       "inbox": str(self.inbox)}],
            "sources": {"project": {"native_bundles": [
                {"path": str(self.root / f"GEN{i}"), "db_id": "DB1", "generation": f"GEN{i}"}
                for i in (3, 4, 5)]}},
            "outbox": [{"peer_device_id": "replica", "descriptor": self.descriptor,
                        "delivery_id": "a" * 32, "bundle": str(self.root / "GEN3")}],
            "awaiting_ack": []})
        write_json(library._publisher_path(self.config), {
            "schema": library.PUBLISHER_SCHEMA,
            "peers": [{"device_id": "owner", "receive_inbox": str(self.inbox)}],
            "sources": {}, "outbox": [], "awaiting_ack": []})


class AckCuts(BoundaryCuts):
    def __enter__(self):
        original_write = library._write_json
        original_unlink = Path.unlink
        original_fsync = library.os.fsync
        original_rmtree = library.shutil.rmtree

        def write(path, value):
            path = Path(path)
            name = None
            if path.name.endswith(".ack.json"):
                name = "receiver_ack_write"
            elif path == self.config.parent / "owner" / "catalog.json":
                name = "owner_catalog_write"
            elif (path == self.config.parent / "owner" / "library.json.publisher.json"
                  and not value.get("outbox") and not value.get("awaiting_ack")):
                name = "owner_ack_state_write"
            return (self.around(name, original_write, path, value) if name
                    else original_write(path, value))

        def unlink(path, *args, **kwargs):
            name = ("receiver_delivery_unlink" if path.suffix == ".tar" else
                    "owner_ack_unlink" if path.name.endswith(".ack.json") else None)
            return (self.around(name, original_unlink, path, *args, **kwargs) if name
                    else original_unlink(path, *args, **kwargs))

        def fsync(fd):
            # `_peer_send`'s first sync is the just-written operation-bound
            # transport temp. Only used in the send-only crash worker mode.
            return self.around("delivery_temp_sync", original_fsync, fd)

        def rmtree(path, *args, **kwargs):
            if Path(path) == self.config.parent / "GEN3":
                return self.around("owner_source_cleanup", original_rmtree, path, *args, **kwargs)
            return original_rmtree(path, *args, **kwargs)

        for obj, name, wrapper in (
            (library, "_write_json", write), (Path, "unlink", unlink),
            (library.os, "fsync", fsync), (library.shutil, "rmtree", rmtree),
        ):
            self.stack.enter_context(patch.object(obj, name, wrapper))
        return self


class PublicationFaultTests(unittest.TestCase):
    def assert_coherent(self, fixture):
        entries = library.status(fixture.config)["entries"]
        self.assertEqual(len(entries), 1)
        replicas = entries[0]["replicas"]
        selected = [r["generation"] for r in replicas]
        self.assertIn(selected, (["GEN2", "GEN1"], ["GEN3", "GEN2"]))
        registry = library.serving_registry(fixture.config)
        self.assertEqual(set(registry), {r["database"] for r in replicas})
        for replica in replicas:
            generation = replica["generation"]
            path = registry[replica["database"]]
            self.assertEqual(path.read_bytes(), f"{generation}:database.db".encode())
            self.assertEqual((path.with_suffix(".bodies") / "body1").read_bytes(),
                             f"{generation}:bodies/body1".encode())
        self.assertEqual(fixture.unknown.read_bytes(), b"not our cleanup inventory")
        return selected

    def assert_retry(self, fixture):
        response = fixture.receive()
        self.assertEqual(response["result"], "activated")
        self.assertEqual(response["ack"]["delivery_id"], "a" * 32)
        self.assertEqual(response["ack"]["receiver_device_id"], "replica")
        self.assertEqual([r["generation"] for r in response["ack"]["replicas"]],
                         ["GEN3", "GEN2"])
        self.assertEqual(self.assert_coherent(fixture), ["GEN3", "GEN2"])
        # Lost acknowledgements must replay the exact immutable bytes and identity.
        self.assertEqual(fixture.receive()["ack"], response["ack"])

    def test_before_after_io_failures_preserve_complete_selection_and_exact_retry(self):
        for operation in BoundaryCuts.OPERATIONS:
            for edge in ("before", "after"):
                cut = operation + ":" + edge
                with self.subTest(cut=cut), tempfile.TemporaryDirectory() as root:
                    fixture = ReceiveFixture(root)
                    with BoundaryCuts(fixture.config, cut) as cuts:
                        with self.assertRaisesRegex(InjectedFailure, cut):
                            fixture.receive()
                    self.assertTrue(cuts.hit)
                    self.assert_coherent(fixture)
                    self.assert_retry(fixture)

    def test_real_process_death_at_distinct_publication_cuts_reopens_and_retries(self):
        # Payload durable / generation crossed / service stopped / selector
        # crossed / selector durable / cleanup completed / ack lost.
        cuts = ("payload_sync:after", "prepared_sync:after", "generation_rename:after",
                "generation_parent_sync:after", "service_stop:after",
                "catalog_file_sync:after", "catalog_replace:after",
                "catalog_parent_sync:after", "service_start:after",
                "cleanup:after", "ack:after")
        for cut in cuts:
            with self.subTest(cut=cut), tempfile.TemporaryDirectory() as root:
                fixture = ReceiveFixture(root)
                result = subprocess.run(
                    [sys.executable, "-B", str(Path(__file__).resolve()), "--crash-worker",
                     str(fixture.config), str(fixture.archive_path), cut],
                    stdout=subprocess.PIPE, stderr=subprocess.PIPE, timeout=15)
                self.assertEqual(result.returncode, 73, result.stderr.decode())
                self.assert_coherent(fixture)
                residue = {p: sorted(str(x.relative_to(p)) for x in p.rglob("*"))
                           for p in fixture.config.parent.rglob(".stage-*")}
                self.assert_retry(fixture)
                # A later operation owns only its own stage. Process-death
                # residue is evidence, not permission for a directory sweep.
                for path, inventory in residue.items():
                    self.assertTrue(path.is_dir())
                    self.assertEqual(sorted(str(x.relative_to(path)) for x in path.rglob("*")),
                                     inventory)

    def test_same_generation_conflict_never_clobbers_existing_evidence(self):
        with tempfile.TemporaryDirectory() as root:
            fixture = ReceiveFixture(root)
            target = fixture.parent / "GEN3"
            target.mkdir()
            marker = target / "evidence"
            marker.write_bytes(b"unrelated preexisting target")
            with self.assertRaises((OSError, library.LibraryError)):
                fixture.receive()
            self.assertEqual(marker.read_bytes(), b"unrelated preexisting target")
            self.assertEqual(self.assert_coherent(fixture), ["GEN2", "GEN1"])

    def test_local_ack_process_death_never_forgets_pending_bytes_or_route(self):
        operations = (
            ("receive", "receiver_ack_write"),
            ("receive", "receiver_delivery_unlink"),
            ("sync", "owner_catalog_write"),
            ("sync", "owner_ack_state_write"),
            ("sync", "owner_ack_unlink"),
            ("sync", "owner_source_cleanup"),
        )
        for action, operation in operations:
            for edge in ("before", "after"):
                cut = operation + ":" + edge
                with self.subTest(cut=cut), tempfile.TemporaryDirectory() as root:
                    fixture = AckFixture(root)
                    self.assertEqual(library.sync(fixture.owner)["pending"], 1)
                    if action == "sync":
                        self.assertEqual(library.receive_inbox(fixture.config)["accepted"], 1)
                    result = subprocess.run(
                        [sys.executable, "-B", str(Path(__file__).resolve()), "--ack-crash-worker",
                         str(fixture.root), action, cut],
                        stdout=subprocess.PIPE, stderr=subprocess.PIPE, timeout=15)
                    self.assertEqual(result.returncode, 73, result.stderr.decode())
                    owner = library.status(fixture.owner)
                    sidecar = library._read_json(library._publisher_path(fixture.owner))
                    for item in sidecar["outbox"] + sidecar["awaiting_ack"]:
                        self.assertTrue(Path(item["bundle"]).is_dir(), "pending delivery lost bytes")
                    selected = [r["generation"] for r in owner["entries"][0]["replicas"]]
                    self.assertIn(selected, (["GEN2", "GEN1"], ["GEN3", "GEN2"]))
                    if not owner["pending"]:
                        self.assertEqual(selected, ["GEN3", "GEN2"], "forgot ack before saving route")
                    self.assert_coherent(fixture)
                    library.receive_inbox(fixture.config)
                    self.assertEqual(library.sync(fixture.owner)["pending"], 0)
                    self.assertEqual(library.status(fixture.owner)["entries"][0]["replicas"],
                                     library.status(fixture.config)["entries"][0]["replicas"])
                    self.assertFalse((fixture.root / "GEN3").exists())
                    self.assertTrue((fixture.root / "GEN4").exists())
                    self.assertTrue((fixture.root / "GEN5").exists())
                    self.assert_coherent(fixture)

    def test_dead_local_send_temp_does_not_poison_exact_delivery_retry(self):
        with tempfile.TemporaryDirectory() as root:
            fixture = AckFixture(root)
            result = subprocess.run(
                [sys.executable, "-B", str(Path(__file__).resolve()), "--ack-crash-worker",
                 str(fixture.root), "send", "delivery_temp_sync:after"],
                stdout=subprocess.PIPE, stderr=subprocess.PIPE, timeout=15)
            self.assertEqual(result.returncode, 73, result.stderr.decode())
            residue = {p: p.read_bytes() for p in fixture.inbox.glob("*.part*")}
            self.assertTrue(residue, "cut must leave a real transport temp")
            self.assertEqual(library.sync(fixture.owner)["pending"], 1)
            self.assertEqual(library.receive_inbox(fixture.config)["accepted"], 1)
            self.assertEqual(library.sync(fixture.owner)["pending"], 0)
            for path, data in residue.items():
                self.assertEqual(path.read_bytes(), data)
            self.assert_coherent(fixture)


if __name__ == "__main__":
    if len(sys.argv) > 1 and sys.argv[1] == "--crash-worker":
        _, _, config, archive, cut = sys.argv
        with BoundaryCuts(config, cut, die=True):
            library.receive(config, Path(archive).read_bytes(),
                            authenticated_peer="owner", include_ack=True)
        raise SystemExit("requested crash cut was not reached")
    if len(sys.argv) > 1 and sys.argv[1] == "--ack-crash-worker":
        _, _, root, action, cut = sys.argv
        root = Path(root)
        with AckCuts(root / "anchor", cut, die=True):
            if action == "receive":
                library.receive_inbox(root / "replica" / "library.json")
            elif action == "sync":
                library.sync(root / "owner" / "library.json")
            elif action == "send":
                library._peer_send({"transport": "local_directory", "inbox": str(root / "inbox")},
                                   (root / "delivery.tar").read_bytes(), "a" * 32)
            else:
                raise AssertionError(action)
        raise SystemExit("requested ACK crash cut was not reached")
    unittest.main()
