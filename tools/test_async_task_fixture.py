#!/usr/bin/env python3
"""Provider-free synthetic fixtures; no authored holdout is executed here."""
from contextlib import closing
import json
import os
from pathlib import Path
import sqlite3
import tempfile
import unittest
from unittest.mock import patch

import async_task_fixture as subject


def card(key, kind="semantic", thread=None):
    return {"key": key, "summary": "A scoped synthetic note.", "body": "Synthetic body.",
            "kind": kind, "thread": thread}


def inventory():
    cards = [card("host-secret-a"), card("host-secret-b", "episode", "test-thread")]
    case = {"id": "private-case-id", "split": "development", "family": "private-family",
            "scene": {"files": {"input/readme.txt": "Public synthetic scene."}},
            "memory_keys": [c["key"] for c in cards],
            "edges": [{"from": cards[0]["key"], "to": cards[1]["key"],
                       "kind": "associative", "weight": .7}], "host": {"gold": "DO NOT EXPOSE"}}
    return case, {c["key"]: c for c in cards}


def create_db(path):
    path.parent.mkdir(parents=True, exist_ok=True)
    with closing(sqlite3.connect(path)) as db, db:
        db.execute("CREATE TABLE cozo (k BLOB PRIMARY KEY, v BLOB)")
        db.execute("INSERT INTO cozo VALUES (?, ?)", (b"key", b"value"))


class FixtureTests(unittest.TestCase):
    def test_pinned_closed_snapshot_clone_and_changed_missing_refusal(self):
        with tempfile.TemporaryDirectory() as tmp:
            root = Path(tmp); source = root / "source.db"; create_db(source)
            source.with_name(source.name + ".mneme.lock").touch()
            bodies = source.with_suffix(".bodies"); bodies.mkdir(); body = bodies / "body"; body.write_text("grounded")
            manifest = root / "snapshot.json"
            record = {"schema":"mneme.ordinary-closed-snapshot.v1", "default_build_sha256":"build",
                      "db":{"path":str(source.resolve()),"sha256":subject.sha(source)},
                      "bodies":subject._body_inventory(bodies)}
            subject._json(manifest, record)
            host = root / "host"; host.mkdir()
            result = subject._clone_closed_snapshot(source, manifest, host, "build")
            self.assertEqual(result["mode"], "organic_closed_snapshot")
            self.assertEqual(subject.logical_cozo(source), subject.logical_cozo(host / ".mneme/codex-memory.db"))
            self.assertEqual((host / ".mneme/codex-memory.bodies/body").read_text(), "grounded")
            for index, change in enumerate(("body_changed", "body_missing", "db_changed")):
                target = root / ("host-" + str(index)); target.mkdir()
                if change == "body_changed": body.write_text("changed")
                elif change == "body_missing": body.unlink()
                else:
                    body.write_text("grounded")
                    with closing(sqlite3.connect(source)) as db, db: db.execute("UPDATE cozo SET v=?",(b"changed",))
                with self.assertRaises(RuntimeError):
                    subject._clone_closed_snapshot(source, manifest, target, "build")
                self.assertFalse((target / ".mneme").exists())

    def test_ordinary_snapshot_fixture_never_seeds_and_startup_is_exact(self):
        with tempfile.TemporaryDirectory() as tmp:
            base = Path(tmp); source = base / "source.db"; create_db(source)
            source.with_name(source.name + ".mneme.lock").touch()
            source.with_suffix(".bodies").mkdir()
            (source.with_suffix(".bodies") / "body").write_text("organic")
            snapshot = base / "snapshot.json"
            subject._json(snapshot, {"schema":"mneme.ordinary-closed-snapshot.v1",
                "default_build_sha256":"build", "db":{"path":str(source.resolve()),"sha256":subject.sha(source)},
                "bodies":subject._body_inventory(source.with_suffix(".bodies"))})
            binary = base / "fake"; binary.write_text("#!/bin/sh\nexit 0\n");binary.chmod(0o700)
            auth = base / "auth"; auth.write_text("{}")
            case = {"scene":{"files":{"task.txt":"new task"}}}
            with patch.object(subject,"artifact_manifest",return_value={"build_inventory":{"sha256":"build"}}), \
                 patch.object(subject,"_seed",side_effect=AssertionError("never reseed")), \
                 patch.object(subject,"_processes",return_value={}), \
                 patch.object(subject,"_run",side_effect=lambda argv,**kw:{"state":"ready" if argv[-1]=="start" else "stopped"}), \
                 patch.object(subject,"cli",return_value={}):
                with subject.fixture(case,{},base / "fixture",mnemed=binary,mcp=binary,codex=binary,auth=auth,
                                     ordinary=True,build_inventory=base / "build.json",
                                     closed_snapshot=source,snapshot_manifest=snapshot,
                                     reader_model="gpt-6.1-sol", librarian_effort="medium",
                                     cache_root=base / "private-cache") as obj:
                    self.assertEqual(obj.manifest["seed"]["mode"],"organic_closed_snapshot")
                    self.assertEqual(obj.before,subject.logical_cozo(source))
                    self.assertEqual(obj.identifiers,{})
                    config = json.loads(obj.hook_config.read_text())
                    self.assertEqual((config["reader_model"], config["librarian_effort"]),
                                     ("gpt-6.1-sol", "medium"))
                    self.assertEqual(obj.env["FASTEMBED_CACHE_DIR"], str((base / "private-cache").resolve()))
                self.assertTrue(obj.cleanup["ok"])

    def test_closed_snapshot_requires_ordinary_manifest_and_exclusive_seed(self):
        for options in ({"closed_snapshot":Path("/source")},
                        {"closed_snapshot":Path("/source"),"snapshot_manifest":Path("/manifest")},
                        {"ordinary":True,"closed_snapshot":Path("/source"),"snapshot_manifest":Path("/manifest"),
                         "seed_template":Path("/seed"),"build_inventory":Path("/build")}):
            with self.subTest(options=options), tempfile.TemporaryDirectory() as tmp:
                with self.assertRaises(RuntimeError):
                    with subject.fixture({}, {}, Path(tmp), mnemed=Path("/missing"),mcp=Path("/missing"),
                                         codex=Path("/missing"),auth=Path("/missing"),**options): pass

    def test_ordinary_cleanup_allows_recording_but_requires_exact_reopen(self):
        for ordinary in (False, True):
            with self.subTest(ordinary=ordinary), tempfile.TemporaryDirectory() as tmp:
                root = Path(tmp); db = root / "db"; db.touch()
                obj = subject.Fixture(root, root / "project", root / "host", root / "home", {},
                    root / "bundle", root / "service", root / "hooks", root / "home-hooks",
                    root / "state", db, root / "native", before={"old":1}, ordinary=ordinary,
                    service_started=True)
                with patch.object(subject, "_processes", return_value={}), \
                     patch.object(subject, "_run", return_value={"state":"stopped"}), \
                     patch.object(subject, "logical_cozo", return_value={"new":2}), \
                     patch.object(subject, "cli", return_value={}):
                    obj.close()
                self.assertFalse(obj.cleanup["persistent_kv_equal"])
                self.assertTrue(obj.cleanup["reopen_kv_equal"])
                self.assertEqual(obj.cleanup["ok"], ordinary)

    def test_ordinary_environment_clears_credentials_and_uses_private_home(self):
        with patch.dict(os.environ, {"CODEX_API_KEY":"secret","CODEX_CONFIG":"/live",
                                    "OPENAI_API_KEY":"secret","HOME":"/live"}):
            env = subject.fixture_env(Path("/private/root"), Path("/bin/native"),
                                      Path("/private/home"), ordinary=True)
        self.assertEqual(env["HOME"], "/private/home")
        self.assertEqual(env["CODEX_HOME"], "/private/home")
        self.assertNotIn("CODEX_API_KEY", env)
        self.assertNotIn("CODEX_CONFIG", env)
        self.assertNotIn("OPENAI_API_KEY", env)

    def test_fresh_default_manifest_pins_actual_binaries_and_rejects_hashing(self):
        with tempfile.TemporaryDirectory() as tmp:
            root = Path(tmp); client = root / "client"; mcp = root / "mcp"
            client.write_text("client"); mcp.write_text("mcp")
            model = root / "cache/models--Xenova--bge-base-en-v1.5"
            (model / "refs").mkdir(parents=True)
            revision = "a" * 40; (model / "refs/main").write_text(revision)
            blob = model / "snapshots" / revision / "onnx/model.onnx"
            blob.parent.mkdir(parents=True); blob.write_text("model")
            manifest = {"schema":"mneme.routing-native-build.v1", "status":"built",
                "features":"default cozo,fastembed,http", "source_pins_unchanged":True,
                "binaries":{"client":{"sha256":subject.sha(client)},"mcp":{"sha256":subject.sha(mcp)}}}
            inventory = root / "build.json"; subject._json(inventory, manifest)
            with patch.object(subject, "CACHE", root / "cache"):
                result = subject.artifact_manifest(client, mcp, build_inventory=inventory)
                self.assertEqual(result["binaries"]["mnemed"]["sha256"], subject.sha(client))
                client.write_text("stale")
                with self.assertRaisesRegex(RuntimeError, "does not match"):
                    subject.artifact_manifest(client, mcp, build_inventory=inventory)
                manifest["features"] = "cozo,http"
                subject._json(inventory, manifest)
                with self.assertRaisesRegex(RuntimeError, "default build"):
                    subject.artifact_manifest(client, mcp, build_inventory=inventory)

    def test_ordinary_advances_scene_home_not_store_after_drain(self):
        with tempfile.TemporaryDirectory() as tmp:
            root = Path(tmp); host = root / "host"; host.mkdir()
            project = host / "project"; project.mkdir(); (project / "old.txt").write_text("old")
            home = root / "home"; home.mkdir(); auth = root / "auth"; auth.write_text("{}")
            (home / "auth.json").symlink_to(auth)
            (home / "hooks.json").write_text("{}")
            config = root / "config.json"; config.write_text("{}")
            obj = subject.Fixture(root, project, host, home, {}, root / "bundle",
                root / "service.json", config, home / "hooks.json", root / "state",
                host / ".mneme/codex-memory.db", root / "native", ordinary=True,
                cache_root=root / "private-cache")
            with self.assertRaises(RuntimeError):
                obj.next_task({"new.txt":"new"}, drain={"drain_completed":False})
            with patch.object(subject, "_processes", return_value={}):
                obj.next_task({"new.txt":"new"}, drain={"drain_completed":True,"usage_unknown":False,"recording_mode":"automatic",
                    "recording_receipt_export":{"errors":[],"truncated":False}})
            self.assertEqual(obj.db, host / ".mneme/codex-memory.db")
            self.assertEqual(list(obj.project.iterdir()), [obj.project / "new.txt"])
            self.assertFalse(project.exists())
            self.assertFalse(home.exists())
            self.assertNotEqual(obj.home, home)
            self.assertEqual(obj.env["CODEX_HOME"], str(obj.home))
            self.assertEqual(obj.env["HOME"], str(obj.home))
            self.assertEqual(obj.env["FASTEMBED_CACHE_DIR"], str((root / "private-cache").resolve()))
            self.assertEqual(json.loads(config.read_text())["reader_auth"], str(obj.home / "auth.json"))

    def test_new_native_manifest_requires_bge_stable_sources_and_binary_hashes(self):
        with tempfile.TemporaryDirectory() as tmp:
            root = Path(tmp).resolve()
            client = root / "client"; client.write_text("client")
            mcp = root / "mcp"; mcp.write_text("mcp")
            source = root / "source.rs"; source.write_text("pinned native source")
            source_inventory = root / "source-before.json"
            subject._json(source_inventory, {"source.rs": subject.sha(source)})
            cache = root / "private-cache"
            model = cache / "models--Xenova--bge-base-en-v1.5"
            (model / "refs").mkdir(parents=True)
            revision = "a" * 40; (model / "refs/main").write_text(revision)
            blob = model / "snapshots" / revision / "onnx/model.onnx"
            blob.parent.mkdir(parents=True); blob.write_text("model")
            manifest = {"exit": 0, "source_stable": True,
                "feature_matrix": {"mnemed": ["cozo", "fastembed"],
                                   "mneme-mcp": ["cozo", "fastembed", "http"]},
                "binaries": {"mnemed": {"sha256": subject.sha(client)},
                             "mneme-mcp": {"sha256": subject.sha(mcp)}}}
            inventory = root / "manifest.json"; subject._json(inventory, manifest)
            with patch.object(subject, "ROOT", root):
                result = subject.artifact_manifest(client, mcp, build_inventory=inventory, cache_root=cache)
                self.assertEqual(result["embedding"]["cache"], str(cache))
                self.assertEqual(result["native_source_inventory"]["sha256"], subject.sha(source_inventory))
                source.write_text("drift")
                with self.assertRaisesRegex(RuntimeError, "source drift"):
                    subject.artifact_manifest(client, mcp, build_inventory=inventory, cache_root=cache)
                source.write_text("pinned native source")
                client.write_text("drift")
                with self.assertRaisesRegex(RuntimeError, "does not match"):
                    subject.artifact_manifest(client, mcp, build_inventory=inventory, cache_root=cache)
                client.write_text("client")
                for field, value in (("source_stable", False), ("exit", 1),
                        ("feature_matrix", {"mnemed": ["cozo"], "mneme-mcp": ["cozo", "http"]})):
                    rejected = {**manifest, field: value}; subject._json(inventory, rejected)
                    with self.subTest(field=field), self.assertRaises(RuntimeError):
                        subject.artifact_manifest(client, mcp, build_inventory=inventory, cache_root=cache)
                subject._json(inventory, manifest)
                subject._json(source_inventory, {"../outside.rs": "a" * 64})
                with self.assertRaisesRegex(RuntimeError, "invalid native source pin"):
                    subject.artifact_manifest(client, mcp, build_inventory=inventory, cache_root=cache)

    def test_scene_contains_only_materialized_files(self):
        case, _ = inventory()
        with tempfile.TemporaryDirectory() as tmp:
            project = Path(tmp) / "project"
            subject.materialize_scene(case, project)
            self.assertEqual([str(p.relative_to(project)) for p in project.rglob("*") if p.is_file()],
                             ["input/readme.txt"])
            self.assertEqual((project / "input/readme.txt").read_text(), "Public synthetic scene.")
            self.assertNotIn("DO NOT EXPOSE", repr(list(project.rglob("*"))))

    def test_scene_paths_cannot_open_host_or_product_config(self):
        for name in ("../secret", "/secret", "a/../secret", ".mneme/db", ".codex/hooks.json",
                     "AGENTS.md", "a/AGENTS.md", "a\\secret"):
            with self.subTest(name=name), tempfile.TemporaryDirectory() as tmp:
                with self.assertRaises(RuntimeError):
                    subject.materialize_scene({"scene": {"files": {name: "no"}}}, Path(tmp) / "project")

    def test_host_inventory_projection_has_no_gold(self):
        case, cards = inventory()
        result = subject.seed_inventory(case, cards)
        self.assertEqual(set(result), {"cards", "edges"})
        for forbidden in ("private-case-id", "development", "private-family", "DO NOT EXPOSE"):
            self.assertNotIn(forbidden, json.dumps(result))

    def test_environment_cannot_inherit_live_routing(self):
        with patch.dict(os.environ, {"MNEME_DB": "/live/store", "MNEME_SERVICE_CONFIG": "/live/config",
                        "MNEME_LIBRARY_CONFIG": "/live/library", "OPENAI_API_KEY": "secret",
                        "HF_HOME": "/wrong/cache", "PYTHONPATH": "/wrong/modules"}):
            env = subject.fixture_env(Path("/private/root"), Path("/native/bin"), Path("/private/home"))
        for key in ("MNEME_DB", "MNEME_SERVICE_CONFIG", "MNEME_LIBRARY_CONFIG", "OPENAI_API_KEY", "HF_HOME", "PYTHONPATH"):
            self.assertNotIn(key, env)
        self.assertEqual(env["CODEX_HOME"], "/private/home")
        self.assertEqual(env["FASTEMBED_CACHE_DIR"], str(subject.CACHE))
        self.assertEqual(env["MNEME_RERANK"], "0")

    def test_ordinary_capture_uses_current_lifecycle_payload_without_active(self):
        case, cards = inventory()
        calls = []
        def cli(binary, host, env, *args, **kwargs):
            calls.append((args, kwargs))
            return {"id":"0"*26,"edition_id":"1"*26}
        with tempfile.TemporaryDirectory() as tmp, patch.object(subject, "cli", side_effect=cli):
            subject._seed(case, cards, Path(tmp), Path("/fake/native"), {}, ordinary=True)
        captures = [kw["payload"] for args, kw in calls if args[:2] == ("capture","add")]
        self.assertEqual(len(captures), 1)
        self.assertEqual(set(captures[0]), {"source","summary","body"})
        self.assertEqual(calls[0][0], ("capture","init"))

    def test_seed_preserves_episode_kind_thread_edges_without_author_keys(self):
        case, cards = inventory()
        calls = []
        def cli(binary, host, env, *args, **kwargs):
            calls.append((args, kwargs))
            return {"id": "0" * 26, "edition_id": "1" * 26}
        with tempfile.TemporaryDirectory() as tmp, patch.object(subject, "cli", side_effect=cli):
            ids = subject._seed(case, cards, Path(tmp), Path("/bin/fake"), {})
        self.assertEqual(calls[0][0], ("capture", "init"))
        self.assertEqual(calls[0][1]["db"].name, "codex-memory.db")
        self.assertFalse(any(args[0] == "episode-upgrade" for args, _ in calls))
        captures = [kw["payload"] for args, kw in calls if args[:2] == ("capture", "add")]
        episodes = [kw["payload"] for args, kw in calls if args[:2] == ("episode", "append")]
        self.assertEqual(len(captures), 1)
        self.assertTrue(captures[0]["active"])
        self.assertEqual(episodes[0]["thread"], "test-thread")
        self.assertEqual(episodes[0]["occurred"], {"kind": "unknown"})
        self.assertEqual(episodes[0]["summary"], cards["host-secret-b"]["summary"])
        self.assertEqual(ids, {"host-secret-a": "0" * 26, "host-secret-b": "1" * 26})
        for payload in captures + episodes:
            self.assertNotIn("host-secret", json.dumps(payload))
        link = next(args for args, kw in calls if args[0] == "link")
        self.assertEqual(link, ("link", "--from", "0" * 26, "--to", "1" * 26,
                                "--kind", "associative", "--weight", "0.7"))

    def test_clone_preserves_exact_kv_and_bodies_and_refuses_changes(self):
        case, cards = inventory()
        with tempfile.TemporaryDirectory() as tmp:
            root = Path(tmp)
            native = root / "native"
            native.write_text("fixture binary")
            template, dest = root / "template", root / "host"
            template.mkdir(); dest.mkdir()
            source = template / ".mneme/codex-memory.db"
            create_db(source)
            subject._json(source.parent / "profile.json", {"schema": "mneme.profile.v1", "mode": "isolated"})
            source.with_suffix(".bodies").mkdir()
            (source.with_suffix(".bodies") / "body.txt").write_text("source body")
            record = {"schema": "mneme.async-task-seed.v1", "inventory_sha256": subject._digest_json(subject.seed_inventory(case, cards)),
                      "identifiers": {"host-secret-a": "0"*26}, "logical_kv": subject.logical_cozo(source),
                      "mnemed_sha256": subject.sha(native)}
            subject._json(template / "seed.json", record)
            self.assertEqual(subject._clone_template(template, dest, case, cards, native), record)
            self.assertEqual(subject.logical_cozo(dest / ".mneme/codex-memory.db"), record["logical_kv"])
            self.assertEqual((dest / ".mneme/codex-memory.bodies/body.txt").read_text(), "source body")
            with closing(sqlite3.connect(source)) as db, db:
                db.execute("UPDATE cozo SET v=?", (b"changed",))
            with self.assertRaisesRegex(RuntimeError, "changed"):
                subject._clone_template(template, dest, case, cards, native)

    def test_installed_collect_process_uses_its_own_modules_and_source_order(self):
        with tempfile.TemporaryDirectory() as tmp:
            root = Path(tmp)
            prefix = root / "bundle"
            lib = prefix / "lib"; lib.mkdir(parents=True)
            (lib / "hook_recall.py").write_text('''
class McpClient:
    pass
def collect_reader(service, cue, project, timeout):
    assert cue == 'synthetic cue'
    assert timeout == 1.5
    return {'outcome':'ok','cards':[{'id': str(i), 'summary':'note'} for i in [3,1,2]],'elapsed_ms':7}
''')
            (lib / "hooks.py").write_text('''
MAX_CONTEXT_BYTES=4096
def _valid_card(c): return True
def _render_cards(cards): return ','.join(c['id'] for c in cards)
''')
            obj = subject.Fixture(root, root / "project", root / "host", root / "home", os.environ.copy(),
                prefix, prefix / "config/service.json", prefix / "config/hooks.json", root / "hooks.json",
                root / "state", root / "db", root / "mnemed", service_started=True)
            result = obj.native_baseline("synthetic cue")
            self.assertEqual(result["context"], "3,1")
            self.assertEqual(result["selected_ids"], ["3", "1"])
            self.assertEqual(result["pool"], result["result"])
            self.assertGreaterEqual(result["blocking_elapsed_ms"], 0)
            self.assertEqual(obj.collect("synthetic cue")["cards"][0]["id"], "3")

    def test_cleanup_tracks_reader_separate_process_group(self):
        prefix = Path("/disposable/bundle")
        rows = {101: {"ppid": 1, "pgid": 101, "command": f"python {prefix}/lib/reader_worker.py --serve --config {prefix}/config/hooks.json --session-id session"},
                102: {"ppid": 101, "pgid": 102, "command": "codex app-server"},
                103: {"ppid": 102, "pgid": 102, "command": "provider child"},
                201: {"ppid": 1, "pgid": 201, "command": "some unrelated process"}}
        self.assertEqual(subject._owned_tree(rows, prefix), {101, 102, 103})
        self.assertEqual(subject._owned_tree(rows, Path("/other/bundle")), set())


    def test_cleanup_recognizes_only_scoped_observer_worker(self):
        prefix = Path("/disposable/bundle")
        worker = (str(subject.ROOT / "tools/async_task_observer.py") + " --bundle-lib "
                  + str(prefix / "lib") + " --trace /host/trace -- --serve --config "
                  + str(prefix / "config/hooks.json") + " --session-id synthetic")
        rows = {101: {"ppid": 1, "pgid": 101, "command": worker},
                102: {"ppid": 101, "pgid": 102, "command": "codex app-server"},
                201: {"ppid": 1, "pgid": 201, "command": worker.replace("--serve", "--hook")}}
        self.assertEqual(subject._owned_tree(rows, prefix), {101, 102})
        self.assertEqual(subject._owned_tree(rows, Path("/other/bundle")), set())

    def test_cli_edge_kind_translation_is_not_a_fixture_rewrite(self):
        case, cards = inventory()
        case["edges"][0]["kind"] = "derived_from"
        calls = []
        def cli(binary, host, env, *args, **kwargs):
            calls.append(args)
            return {"id": "0" * 26, "edition_id": "1" * 26}
        with tempfile.TemporaryDirectory() as tmp, patch.object(subject, "cli", side_effect=cli):
            subject._seed(case, cards, Path(tmp), Path("/bin/fake"), {})
        self.assertIn("derived-from", calls[-1])
        self.assertEqual(case["edges"][0]["kind"], "derived_from")

    def test_fixture_enter_does_not_collect_and_stops_service_on_exception(self):
        case, cards = inventory()
        with tempfile.TemporaryDirectory() as tmp:
            base = Path(tmp).resolve()
            binary = base / "native"
            binary.write_text("#!/bin/sh\nexit 0\n"); binary.chmod(0o700)
            auth = base / "auth"; auth.write_text("{}")
            calls = []
            def seed(case, cards, host, native, env):
                create_db(host / ".mneme/codex-memory.db")
                subject._json(host / ".mneme/profile.json", {"schema":"mneme.profile.v1","mode":"isolated"})
                return {"host-secret-a":"0"*26,"host-secret-b":"1"*26}
            def run(argv, **kwargs):
                calls.append(argv)
                return {"state": "ready" if argv[-1] == "start" else "stopped"}
            with patch.object(subject, "artifact_manifest", return_value={}), patch.object(subject, "_seed", side_effect=seed), \
                 patch.object(subject, "_run", side_effect=run), patch.object(subject, "_processes", return_value={}), \
                 patch.object(subject, "cli", return_value={}):
                with self.assertRaisesRegex(ValueError, "actor failed"):
                    with subject.fixture(case, cards, base / "arm", mnemed=binary, mcp=binary,
                                         codex=binary, auth=auth, reader_model="gpt-6.1-sol") as obj:
                        self.assertEqual(obj.manifest["config_revision"], subject.install.CONFIG_REVISION)
                        self.assertEqual(obj.manifest["readiness"]["embedder_state"], "lazy_not_exercised")
                        self.assertFalse((obj.host_project / ".codex").exists())
                        self.assertTrue(obj.home_hooks.is_file())
                        self.assertEqual(set(obj.project.iterdir()), {obj.project / "input"})
                        raise ValueError("actor failed")
            self.assertEqual([argv[-1] for argv in calls], ["start", "stop"])
            self.assertTrue(obj.cleanup["persistent_kv_equal"])
            self.assertTrue(obj.cleanup["store_lease_reopen"])
            self.assertTrue(obj.cleanup["workers_and_readers_gone"])


if __name__ == "__main__":
    unittest.main()
