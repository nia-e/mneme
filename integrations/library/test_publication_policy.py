"""Publication authority narrowed by bounded native metadata; no live stores."""

import copy
import io
import json
import os
from pathlib import Path
import socket
import subprocess
import sys
import time
from types import SimpleNamespace
import unittest
from unittest.mock import Mock, patch

sys.path.insert(0, str(Path(__file__).parent))
import library
import publication_policy
import test_library as library_fixtures

DB = "01M3CPEVDTM9PP4HJTMMBZ8HR9"
OTHER_DB = "01M3CV5EZP4HWAK56YJCMZQG2Y"


def selection(root, catalog):
    return {"schema": "mneme.config-selection.v1", "digest": "a" * 64,
            "cwd": str(Path(root).resolve()), "profile": "default",
            "libraries": {"arbitrary-local-name": {"catalog": str(catalog)}},
            "stores": [{"name": "project-memory", "active": True, "admitted": True,
                        "eligible": {"read": True},
                        "store": {"id": DB, "owner": {"db": "project"}}}]}


class PublicationFixtures(unittest.TestCase):
    """Reuse only helpers, not the existing runnable test inventory."""

    def setUp(self):
        library_fixtures.LibraryTests.setUp(self)
        self.catalog = self.owner / "catalog.json"
        self.memory = self.root / "memory.toml"
        self.binary = self.root / "native-mnemed"
        self.policy = publication_policy.NativePublicationPolicy(self.binary, self.memory)
        self.selected = selection(self.project, self.catalog)

    def _select(self, value=None):
        return patch.object(self.policy, "_select", return_value=self.selected if value is None else value)

    def _allows(self):
        return self.policy.allows(self.project, self.catalog, DB, "project")

    def _enroll(self):
        self.assertTrue(library.enroll_project(self.owner_config, self.project, DB))
        return library.status(self.owner_config)["entries"][0]

    def _bundle(self):
        return library_fixtures.LibraryTests._bundle(self, db_id=DB)

    def _state(self):
        return {path: path.read_bytes() if path.exists() else None for path in
                (self.owner_config, self.catalog, library._publisher_path(self.owner_config))}

    def _publisher(self):
        return json.loads(library._publisher_path(self.owner_config).read_text())

    def _write_publisher(self, publisher):
        library_fixtures.write(library._publisher_path(self.owner_config), publisher)


class NativePublicationPolicyTests(PublicationFixtures):
    def test_default_exact_catalog_and_owner_identity_is_allowed(self):
        with self._select() as select:
            self.assertTrue(self._allows())
        select.assert_called_once_with(self.project.resolve())

    def test_profiles_and_each_required_grant_are_independent(self):
        changes = {
            "private": lambda v: v.update(profile="private"),
            "isolated": lambda v: v.update(profile="isolated"),
            "no library": lambda v: v.update(libraries={}),
            "other catalog": lambda v: v["libraries"]["arbitrary-local-name"].update(
                catalog=str(self.root / "different-catalog.json")),
            "no store": lambda v: v.update(stores=[]),
            "inactive": lambda v: v["stores"][0].update(active=False),
            "unreadable": lambda v: v["stores"][0]["eligible"].update(read=False),
            "not admitted": lambda v: v["stores"][0].update(
                active=False, admitted=False, eligible={"read": False}),
            "other identity": lambda v: v["stores"][0]["store"].update(id=OTHER_DB),
            "other alias": lambda v: v["stores"][0]["store"]["owner"].update(db="user"),
        }
        for name, change in changes.items():
            with self.subTest(name=name):
                value = copy.deepcopy(self.selected)
                change(value)
                with self._select(value):
                    self.assertFalse(self._allows())

    def test_catalog_identity_is_canonical_path_not_library_alias(self):
        self.catalog.write_text("{}")
        alias = self.root / "catalog-alias.json"
        try:
            alias.symlink_to(self.catalog)
        except OSError as error:
            if library.host_files.WINDOWS and getattr(error, "winerror", None) == 1314:
                self.skipTest("Windows symlink creation requires Developer Mode or privilege")
            raise
        self.selected["libraries"] = {"not-personal": {"catalog": str(alias)}}
        with self._select():
            self.assertTrue(self._allows())

    def test_invalid_consumed_projection_is_unavailable_not_denial(self):
        changes = {
            "schema": lambda v: v.update(schema="mneme.config-selection.v99"),
            "digest": lambda v: v.update(digest="z" * 64),
            "profile": lambda v: v.update(profile="unknown"),
            "cwd": lambda v: v.update(cwd=str(self.replica)),
            "relative cwd": lambda v: v.update(cwd="."),
            "libraries type": lambda v: v.update(libraries=[]),
            "relative catalog": lambda v: v["libraries"]["arbitrary-local-name"].update(catalog="catalog.json"),
            "library shape": lambda v: v.update(libraries={"library": None}),
            "invalid library name": lambda v: v.update(libraries={"invalid/alias": {"catalog": str(self.catalog)}}),
            "long library name": lambda v: v.update(libraries={"x" * 129: {"catalog": str(self.catalog)}}),
            "stores type": lambda v: v.update(stores={}),
            "row shape": lambda v: v.update(stores=[None]),
            "invalid store name": lambda v: v["stores"][0].update(name="invalid/alias"),
            "long store name": lambda v: v["stores"][0].update(name="x" * 129),
            "missing admitted": lambda v: v["stores"][0].pop("admitted"),
            "numeric active": lambda v: v["stores"][0].update(active=1),
            "numeric admitted": lambda v: v["stores"][0].update(admitted=1),
            "numeric read": lambda v: v["stores"][0]["eligible"].update(read=1),
            "invalid identity": lambda v: v["stores"][0]["store"].update(id="DB1"),
            "missing owner": lambda v: v["stores"][0]["store"].pop("owner"),
            "empty alias": lambda v: v["stores"][0]["store"]["owner"].update(db=""),
            "invalid alias": lambda v: v["stores"][0]["store"]["owner"].update(db="invalid/alias"),
            "long alias": lambda v: v["stores"][0]["store"]["owner"].update(db="x" * 129),
            "active unadmitted": lambda v: v["stores"][0].update(admitted=False),
            "duplicate row": lambda v: v["stores"].append(copy.deepcopy(v["stores"][0])),
        }
        for profile in ("default", "private"):
            for name, change in changes.items():
                with self.subTest(profile=profile, name=name):
                    value = copy.deepcopy(self.selected)
                    value["profile"] = profile
                    change(value)
                    with self._select(value), self.assertRaises(ValueError):
                        self._allows()

    def test_ambiguous_database_identity_is_not_arbitrarily_selected(self):
        second = copy.deepcopy(self.selected["stores"][0])
        second["name"] = "second-name"
        self.selected["stores"].append(second)
        with self._select(), self.assertRaisesRegex(ValueError, "ambiguous"):
            self._allows()

    def test_projection_collection_bounds_fail_before_eligibility(self):
        for field, value in (("libraries", {str(i): {"catalog": str(self.catalog)} for i in range(33)}),
                             ("stores", [self.selected["stores"][0]] * 65)):
            with self.subTest(field=field):
                selected = copy.deepcopy(self.selected)
                selected[field] = value
                with self._select(selected), self.assertRaises(ValueError):
                    self._allows()

    def test_noncanonical_db_identity_is_rejected_before_helper(self):
        for identity in ("DB1", DB.lower(), "8" + DB[1:], None):
            with self.subTest(identity=identity), self._select() as select:
                with self.assertRaises(ValueError):
                    self.policy.allows(self.project, self.catalog, identity, "project")
                select.assert_not_called()

    def test_policy_paths_must_be_explicit_and_absolute(self):
        for binary, config in (("mnemed", self.memory), (self.binary, "memory.toml")):
            with self.subTest(binary=binary, config=config), self.assertRaises(ValueError):
                publication_policy.NativePublicationPolicy(binary, config)


@unittest.skipIf(library.host_files.WINDOWS, "executable-script fixture is Unix-only")
class NativePublicationHelperTests(PublicationFixtures):
    def _helper(self, body):
        self.binary.write_text("#!" + sys.executable + "\n" + body)
        self.binary.chmod(0o700)

    def test_real_helper_gets_only_explicit_metadata_selection_arguments(self):
        recorded = self.root / "argv.json"
        self._helper("import json, sys\nfrom pathlib import Path\n"
                     f"Path({str(recorded)!r}).write_text(json.dumps(sys.argv[1:]))\n"
                     f"print(json.dumps({self.selected!r}))\n")
        self.assertTrue(self._allows())
        self.assertEqual(json.loads(recorded.read_text()),
                         ["--json", "config", "select", "--config", str(self.memory),
                          "--cwd", str(self.project.resolve())])

    def test_missing_helper_is_unavailable(self):
        with self.assertRaises(OSError):
            self._allows()

    def test_malformed_duplicate_and_non_json_helper_output_fail(self):
        for output in ("not JSON", '{"schema":1,"schema":2}', '{"schema":NaN}'):
            with self.subTest(output=output):
                self._helper(f"print({output!r})\n")
                with self.assertRaises(ValueError):
                    self._allows()

    def test_nonzero_helper_does_not_admit_even_valid_stdout(self):
        self._helper(f"import json, sys\nprint(json.dumps({self.selected!r}))\nsys.exit(1)\n")
        with self.assertRaises(ValueError):
            self._allows()

    def test_oversized_helper_output_stops_at_bound(self):
        self._helper("import sys\nsys.stdout.write('x' * 600000)\n")
        with self.assertRaisesRegex(ValueError, "byte limit"):
            self._allows()

    def test_unresponsive_helper_has_deadline(self):
        self._helper("import time\ntime.sleep(60)\n")
        with patch.object(publication_policy, "SELECTION_TIMEOUT", .1):
            with self.assertRaises((TimeoutError, subprocess.TimeoutExpired)):
                self._allows()

    def test_helper_replacement_during_selection_is_unavailable(self):
        self._helper("import json\nfrom pathlib import Path\n"
                     f"p = Path({str(self.binary)!r})\np.write_text(p.read_text() + '# changed\\n')\n"
                     f"print(json.dumps({self.selected!r}))\n")
        with self.assertRaisesRegex(ValueError, "helper changed"):
            self._allows()

    def test_aggregate_successful_output_exhausts_bytes_before_another_spawn(self):
        payload = json.dumps(self.selected) + "\n"
        self._helper(f"import sys\nsys.stdout.write({payload!r})\n")
        with patch.object(publication_policy, "MAX_SELECTION_WORK_BYTES", 2 * len(payload.encode())):
            self.policy = publication_policy.NativePublicationPolicy(self.binary, self.memory)
        self.assertTrue(self._allows())
        self.assertTrue(self._allows())
        self.assertEqual(self.policy._remaining_bytes, 0)
        with patch.object(publication_policy.host_process, "spawn", side_effect=AssertionError("spent byte budget")):
            with self.assertRaisesRegex(ValueError, "work budget exhausted"):
                self._allows()

    def test_aggregate_failed_output_is_charged_and_cannot_buy_a_retry(self):
        payload = json.dumps(self.selected) + "\n"
        self._helper(f"import sys\nsys.stdout.write({payload!r})\n")
        with patch.object(publication_policy, "MAX_SELECTION_WORK_BYTES", len(payload.encode()) + 8):
            self.policy = publication_policy.NativePublicationPolicy(self.binary, self.memory)
        self.assertTrue(self._allows())
        with self.assertRaisesRegex(ValueError, "byte limit"):
            self._allows()
        self.assertLessEqual(self.policy._remaining_bytes, 0)
        with patch.object(publication_policy.host_process, "spawn", side_effect=AssertionError("retry after byte failure")):
            with self.assertRaisesRegex(ValueError, "work budget exhausted"):
                self._allows()

    def test_aggregate_elapsed_time_is_charged_across_successful_selections(self):
        self._helper(f"import json\nprint(json.dumps({self.selected!r}))\n")
        with patch.object(publication_policy, "MAX_SELECTION_WORK_SECONDS", 20):
            self.policy = publication_policy.NativePublicationPolicy(self.binary, self.memory)
        # Charge two ten-second calls without sleeping or replacing the clock
        # used internally by host_process's real bounded pipe.
        for remaining in (10, 0):
            started = time.monotonic()
            clock = SimpleNamespace(monotonic=Mock(side_effect=[started, started, started + 10]))
            with patch.object(publication_policy, "time", clock):
                self.assertTrue(self._allows())
            self.assertEqual(self.policy._remaining_seconds, remaining)
        with patch.object(publication_policy.host_process, "spawn", side_effect=AssertionError("spent time budget")):
            with self.assertRaisesRegex(ValueError, "work budget exhausted"):
                self._allows()

    def test_aggregate_time_caps_a_failed_selection_and_prevents_another_spawn(self):
        self._helper("import time\ntime.sleep(60)\n")
        with patch.object(publication_policy, "MAX_SELECTION_WORK_SECONDS", .1):
            self.policy = publication_policy.NativePublicationPolicy(self.binary, self.memory)
        with self.assertRaises((TimeoutError, subprocess.TimeoutExpired)):
            self._allows()
        self.assertLessEqual(self.policy._remaining_seconds, 0)
        with patch.object(publication_policy.host_process, "spawn", side_effect=AssertionError("retry after time failure")):
            with self.assertRaisesRegex(ValueError, "work budget exhausted"):
                self._allows()


@unittest.skipUnless(os.environ.get("MNEME_NATIVE_POLICY_TEST_CLI"),
                     "set MNEME_NATIVE_POLICY_TEST_CLI to test an explicit native artifact")
class NativePublicationArtifactTests(PublicationFixtures):
    def test_native_selection_happy_private_unreadable_and_missing_library(self):
        self.policy = publication_policy.NativePublicationPolicy(
            os.environ["MNEME_NATIVE_POLICY_TEST_CLI"], self.memory)
        listener = socket.socket()
        self.addCleanup(listener.close)
        listener.bind(("127.0.0.1", 0))
        listener.listen()
        listener.settimeout(.05)
        port = listener.getsockname()[1]
        overlay = self.project / ".mneme" / "memory.toml"
        overlay.parent.mkdir()

        def configure(*, profile="default", read=True, allow_library=True):
            self.memory.write_text(
                'schema = "mneme.config.v1"\n'
                '[stores.project-memory]\n'
                f'id = "{DB}"\n'
                f'owner = {{ url = "http://127.0.0.1:{port}", db = "project" }}\n'
                'active = { workspace = "project", profiles = ["default", "private"], '
                f'roots = [{json.dumps(str(self.project.resolve()))}] }}\n'
                f'permissions = {{ read = {str(read).lower()}, write = false }}\n'
                '[libraries.publisher]\n'
                f'catalog = {json.dumps(str(self.catalog))}\n'
                'profiles = ["default", "private"]\n')
            overlay.write_text('schema = "mneme.config.v1"\n[workspace]\n'
                               f'profile = "{profile}"\n'
                               f'allow_libraries = {json.dumps(["publisher"] if allow_library else [])}\n')

        for name, settings, expected in (
                ("default", {}, True),
                ("private", {"profile": "private"}, False),
                ("unreadable", {"read": False}, False),
                ("library omitted", {"allow_library": False}, False)):
            with self.subTest(policy=name):
                configure(**settings)
                self.assertEqual(self._allows(), expected)
        with self.assertRaises(socket.timeout):
            listener.accept()


@unittest.skipIf(library.host_files.WINDOWS, "snapshot relay requires Unix directory durability")
class PublisherPolicyIntegrationTests(PublicationFixtures):
    def test_native_allowance_does_not_enroll_a_local_registration(self):
        self.assertTrue(library.register_local_project(self.owner_config, self.project, DB,
                        database="project", owner_endpoint="http://127.0.0.1:18001"))
        entry = library.status(self.owner_config)["entries"][0]
        with self._select(), patch.object(library, "_snapshot", side_effect=AssertionError("not enrolled")):
            result = library.once(self.owner_config, policy=self.policy)
        self.assertEqual(set(result), {"delivery", "receive"})
        self.assertEqual(self._publisher()["sources"], {})
        self.assertEqual(self._publisher()["outbox"], [])
        with self._select():
            with self.assertRaisesRegex(library.LibraryError, "not enrolled"):
                library._snapshot(self.owner_config, entry["project_id"], policy=self.policy)
            with self.assertRaisesRegex(library.LibraryError, "not enrolled"):
                library.publish(self.owner_config, entry["project_id"], self._bundle(), policy=self.policy)

    def test_enrollment_requires_native_grant_before_persistent_changes(self):
        before = self._state()
        for profile in ("private", "isolated"):
            self.selected["profile"] = profile
            with self.subTest(profile=profile), self._select():
                self.assertFalse(library.enroll_project(self.owner_config, self.project, DB,
                                 owner_endpoint="http://127.0.0.1:18001", policy=self.policy))
                self.assertEqual(self._state(), before)
        self.selected["profile"] = "default"
        with self._select():
            self.assertTrue(library.enroll_project(self.owner_config, self.project, DB, policy=self.policy))
        self.assertEqual(len(self._publisher()["sources"]), 1)

    def test_native_explicit_binding_does_not_consult_legacy_profile(self):
        library_fixtures.write(self.project / ".mneme" / "profile.json",
                               {"schema": "mneme.profile.v1", "mode": "private"})
        with self._select(), patch.object(library, "_eligible", side_effect=AssertionError("legacy fallback")):
            self.assertTrue(library.enroll_project(self.owner_config, self.project, DB, policy=self.policy))

    def test_native_failure_preserves_enrollment_and_pending_bundle(self):
        entry = self._enroll()
        bundle = self._bundle()
        publisher = self._publisher()
        publisher["outbox"][0]["bundle"] = str(bundle)
        self._write_publisher(publisher)
        before = self._state()
        operations = {
            "enroll": lambda: library.enroll_project(self.owner_config, self.project, DB, policy=self.policy),
            "snapshot": lambda: library._snapshot(self.owner_config, entry["project_id"], policy=self.policy),
            "publish": lambda: library.publish(self.owner_config, entry["project_id"], bundle, policy=self.policy),
            "sync": lambda: library.sync(self.owner_config, policy=self.policy),
            "once": lambda: library.once(self.owner_config, policy=self.policy),
        }
        for name, operation in operations.items():
            with self.subTest(operation=name), \
                    patch.object(self.policy, "_select", side_effect=ValueError("invalid native selection")), \
                    patch.object(library, "_peer_send", side_effect=AssertionError("unavailable is not authority")), \
                    patch.object(library.subprocess, "run", side_effect=AssertionError("must not snapshot")):
                with self.assertRaises(library.LibraryError):
                    operation()
                self.assertEqual(self._state(), before)
                self.assertTrue(bundle.is_dir())

    def test_private_and_isolated_deny_snapshot_and_publish_before_work(self):
        entry = self._enroll()
        before = self._state()
        for profile in ("private", "isolated"):
            self.selected["profile"] = profile
            operations = (
                lambda: library._snapshot(self.owner_config, entry["project_id"], policy=self.policy),
                lambda: library.publish(self.owner_config, entry["project_id"], self.root / "absent-bundle",
                                        policy=self.policy),
            )
            for operation in operations:
                with self.subTest(profile=profile, operation=operation), self._select(), \
                        patch.object(library, "validate_bundle", side_effect=AssertionError("denied body read")), \
                        patch.object(library.subprocess, "run", side_effect=AssertionError("denied snapshot")), \
                        patch.object(library, "_peer_send", side_effect=AssertionError("denied publish")):
                    with self.assertRaises(library.LibraryError):
                        operation()
                    self.assertEqual(self._state(), before)

    def test_missing_native_helper_preserves_queue_instead_of_legacy_fallback(self):
        self._enroll()
        before = self._state()
        with patch.object(library, "_eligible", side_effect=AssertionError("native failure fell back")), \
                patch.object(library, "_peer_send", side_effect=AssertionError("unavailable helper sent queue")):
            with self.assertRaises(library.LibraryError):
                library.sync(self.owner_config, policy=self.policy)
        self.assertEqual(self._state(), before)

    def test_later_source_failure_does_not_apply_an_earlier_withdrawal(self):
        self._enroll()
        second = self.root / "second-project"
        second.mkdir()
        self.assertTrue(library.enroll_project(self.owner_config, second, OTHER_DB))
        before = self._state()
        self.selected["profile"] = "private"
        with patch.object(self.policy, "_select", side_effect=[self.selected, ValueError("unavailable")]), \
                patch.object(library, "_peer_send", side_effect=AssertionError("partial preflight sent")):
            with self.assertRaises(library.LibraryError):
                library.sync(self.owner_config, policy=self.policy)
        self.assertEqual(self._state(), before)

    def test_publish_second_selection_failure_preserves_original_announcement(self):
        entry = self._enroll()
        bundle = self._bundle()
        self.assertIsNone(self._publisher()["outbox"][0]["bundle"])
        before = self._state()
        with patch.object(self.policy, "_select", side_effect=[self.selected, ValueError("unavailable")]) as select, \
                patch.object(library, "_queue", side_effect=AssertionError("superseded before complete preflight")), \
                patch.object(library, "_peer_send", side_effect=AssertionError("partial preflight sent")):
            with self.assertRaises(library.LibraryError):
                library.publish(self.owner_config, entry["project_id"], bundle, policy=self.policy)
        self.assertEqual(select.call_count, 2)
        self.assertEqual(self._state(), before)
        self.assertTrue(bundle.is_dir())

    def test_publish_does_not_reenter_policy_after_superseding_queue(self):
        entry = self._enroll()
        bundle = self._bundle()
        with patch.object(self.policy, "_select", side_effect=[self.selected, self.selected,
                          ValueError("selection after mutation")]) as select, \
                patch.object(library, "sync", side_effect=AssertionError("publish reopened sync")), \
                patch.object(library, "_peer_send", return_value={}) as send:
            result = library.publish(self.owner_config, entry["project_id"], bundle, policy=self.policy)
        self.assertEqual(result["delivered"], 1)
        self.assertEqual(select.call_count, 2)
        send.assert_called_once()
        self.assertEqual(self._publisher()["awaiting_ack"][0]["bundle"], str(bundle.resolve()))

    def test_malformed_native_names_and_aliases_never_withdraw_or_send(self):
        self._enroll()
        before = self._state()
        for bad_name in ("invalid/alias", "x" * 129):
            for field in ("library", "store", "database"):
                for profile in ("default", "private"):
                    value = copy.deepcopy(self.selected)
                    value["profile"] = profile
                    if field == "library":
                        value["libraries"] = {bad_name: {"catalog": str(self.catalog)}}
                    elif field == "store":
                        value["stores"][0]["name"] = bad_name
                    else:
                        value["stores"][0]["store"]["owner"]["db"] = bad_name
                    with self.subTest(name=bad_name, field=field, profile=profile), self._select(value), \
                            patch.object(library, "_peer_send", side_effect=AssertionError("malformed selection sent")):
                        with self.assertRaises(library.LibraryError):
                            library.sync(self.owner_config, policy=self.policy)
                        self.assertEqual(self._state(), before)

    def test_revocation_supersedes_queued_bundle_with_withdrawal_before_send(self):
        self._enroll()
        bundle = self._bundle()
        publisher = self._publisher()
        publisher["outbox"][0]["bundle"] = str(bundle)
        self._write_publisher(publisher)
        self.selected["stores"][0]["eligible"]["read"] = False
        with self._select(), patch.object(library, "_make_archive", wraps=library._make_archive) as archive, \
                patch.object(library, "_peer_send", return_value={}):
            result = library.sync(self.owner_config, policy=self.policy)
        self.assertEqual(result["delivered"], 1)
        self.assertEqual(archive.call_count, 1)
        self.assertTrue(archive.call_args.args[0]["withdrawn"])
        self.assertIsNone(archive.call_args.args[1])
        entry = library.status(self.owner_config)["entries"][0]
        self.assertTrue(entry["withdrawn"])
        self.assertEqual(entry["revision"], 2)
        self.assertTrue(bundle.is_dir())

    def test_queued_descriptor_and_source_identity_are_checked_before_send(self):
        entry = self._enroll()
        original = self._publisher()
        changes = {
            "source missing": lambda p: p["sources"].clear(),
            "source root missing": lambda p: p["sources"][entry["project_id"]].pop("project_root"),
            "source root mismatches": lambda p: p["sources"][entry["project_id"]].update(project_root=str(self.replica)),
            "source alias mismatches": lambda p: p["sources"][entry["project_id"]].update(database="user"),
            "queued identity mismatches": lambda p: p["outbox"][0]["descriptor"].update(db_id=OTHER_DB),
            "queued alias mismatches": lambda p: p["outbox"][0]["descriptor"].update(database="user"),
            "queued owner mismatches": lambda p: p["outbox"][0]["descriptor"].update(owner_device_id="other"),
        }
        for name, change in changes.items():
            publisher = copy.deepcopy(original)
            change(publisher)
            self._write_publisher(publisher)
            before = self._state()
            with self.subTest(name=name), self._select(), \
                    patch.object(library, "_peer_send", side_effect=AssertionError("invalid source sent")):
                with self.assertRaises(library.LibraryError):
                    library.sync(self.owner_config, policy=self.policy)
                self.assertEqual(self._state(), before)

    def test_direct_snapshot_and_publish_require_exact_source_before_any_work(self):
        entry = self._enroll()
        original = self._publisher()
        changes = {
            "root missing": lambda s: s.pop("project_root"),
            "root relative": lambda s: s.update(project_root="project"),
            "root mismatches": lambda s: s.update(project_root=str(self.replica)),
            "alias missing": lambda s: s.pop("database"),
            "alias mismatches": lambda s: s.update(database="user"),
        }
        operations = {
            "snapshot": lambda: library._snapshot(self.owner_config, entry["project_id"], policy=self.policy),
            "publish": lambda: library.publish(self.owner_config, entry["project_id"],
                                               self.root / "absent-bundle", policy=self.policy),
        }
        for name, change in changes.items():
            for operation, invoke in operations.items():
                publisher = copy.deepcopy(original)
                change(publisher["sources"][entry["project_id"]])
                self._write_publisher(publisher)
                before = self._state()
                with self.subTest(source=name, operation=operation), self._select() as select, \
                        patch.object(library.subprocess, "run", side_effect=AssertionError("unbound snapshot")), \
                        patch.object(library, "_queue", side_effect=AssertionError("unbound publication")), \
                        patch.object(library, "validate_bundle", side_effect=AssertionError("unbound body read")):
                    with self.assertRaises(library.LibraryError):
                        invoke()
                    select.assert_not_called()
                    self.assertEqual(self._state(), before)

    def test_awaiting_ack_is_not_an_exemption_from_native_source_checks(self):
        self._enroll()
        publisher = self._publisher()
        publisher["awaiting_ack"] = publisher.pop("outbox")
        publisher["outbox"] = []
        publisher["awaiting_ack"][0]["descriptor"]["db_id"] = OTHER_DB
        self._write_publisher(publisher)
        before = self._state()
        with self._select(), patch.object(library, "_peer_send", side_effect=AssertionError("unchecked ack queue")):
            with self.assertRaises(library.LibraryError):
                library.sync(self.owner_config, policy=self.policy)
        self.assertEqual(self._state(), before)

    def test_withdrawal_exemption_never_allows_queued_snapshot_bytes(self):
        self._enroll()
        original = self._publisher()
        bundle = self._bundle()
        for queue in ("outbox", "awaiting_ack"):
            publisher = copy.deepcopy(original)
            item = publisher["outbox"].pop()
            item["descriptor"]["withdrawn"] = True
            item["bundle"] = str(bundle)
            publisher.setdefault(queue, []).append(item)
            self._write_publisher(publisher)
            before = self._state()
            with self.subTest(queue=queue), \
                    patch.object(self.policy, "_select", side_effect=AssertionError("malformed queue selected")), \
                    patch.object(library, "_make_archive", side_effect=AssertionError("withdrawal archived body bytes")), \
                    patch.object(library, "_peer_send", side_effect=AssertionError("withdrawal sent body bytes")):
                with self.assertRaisesRegex(library.LibraryError, "withdrawal must not carry snapshot bytes"):
                    library.sync(self.owner_config, policy=self.policy)
                self.assertEqual(self._state(), before)
                self.assertTrue(bundle.is_dir())

    def test_snapshot_keeps_old_binary_endpoint_alias_and_replica_path(self):
        with self._select():
            self.assertTrue(library.enroll_project(self.owner_config, self.project, DB,
                            owner_endpoint={"url": "http://127.0.0.1:18001", "token_env": "OWNER_TOKEN"},
                            policy=self.policy))
        entry = library.status(self.owner_config)["entries"][0]
        bundle = self._bundle()
        publisher = self._publisher()
        publisher["snapshot"] = {"mnemed": "/old-pinned-runtime/bin/mnemed",
                                 "remote_url": "http://127.0.0.1:19999"}
        self._write_publisher(publisher)
        replica_before = library._replica_path(self.owner_config, "a", entry["project_id"], "GEN1")
        response = subprocess.CompletedProcess([], 0, stdout=json.dumps(
            {"bundle": str(bundle), "db_id": DB, "generation": "GEN1"}).encode())
        with self._select(), patch.object(library.subprocess, "run", return_value=response) as run:
            self.assertEqual(library._snapshot(self.owner_config, entry["project_id"], policy=self.policy), bundle)
        run.assert_called_once_with(
            ["/old-pinned-runtime/bin/mnemed", "--json", "--remote", "http://127.0.0.1:18001",
             "--remote-token-env", "OWNER_TOKEN", "--remote-db", "project", "snapshot", "create"],
            capture_output=True, timeout=120, check=False)
        self.assertEqual(self._publisher()["snapshot"], publisher["snapshot"])
        self.assertEqual(library._replica_path(self.owner_config, "a", entry["project_id"], "GEN1"), replica_before)

    def test_no_policy_preserves_legacy_profile_and_never_spawns_helper(self):
        with patch.object(publication_policy.host_process, "spawn", side_effect=AssertionError("implicit helper")):
            self._enroll()
            library_fixtures.write(self.project / ".mneme" / "profile.json",
                                   {"schema": "mneme.profile.v1", "mode": "private"})
            with patch.object(library, "_peer_send", return_value={}):
                library.sync(self.owner_config)
        self.assertTrue(library.status(self.owner_config)["entries"][0]["withdrawn"])


class PublicationPolicyCliTests(PublicationFixtures):
    def test_cli_requires_both_binding_flags_before_command_execution(self):
        for flag, value in (("--native-cli", self.binary), ("--memory-config", self.memory)):
            with self.subTest(flag=flag), patch.object(sys, "stderr", io.StringIO()), \
                    patch.object(library, "status", side_effect=AssertionError("partial binding dispatched")):
                with self.assertRaises(SystemExit) as error:
                    library.main(["--config", str(self.owner_config), flag, str(value), "status"])
                self.assertEqual(error.exception.code, 2)

    def test_cli_paired_binding_reaches_publisher(self):
        with patch.object(library, "once", return_value={}) as once, patch.object(sys, "stdout", io.StringIO()):
            self.assertEqual(library.main(["--config", str(self.owner_config),
                             "--native-cli", str(self.binary), "--memory-config", str(self.memory), "once"]), 0)
        once.assert_called_once()
        self.assertEqual(once.call_args.args, (self.owner_config,))
        policy = once.call_args.kwargs["policy"]
        self.assertIsInstance(policy, publication_policy.NativePublicationPolicy)
        self.assertEqual(policy.native_cli, self.binary)
        self.assertEqual(policy.memory_config, self.memory)

    def test_cli_without_binding_does_not_execute_metadata_helper(self):
        with patch.object(publication_policy.host_process, "spawn", side_effect=AssertionError("implicit native selection")), \
                patch.object(sys, "stdout", io.StringIO()):
            self.assertEqual(library.main(["--config", str(self.owner_config), "status"]), 0)


if __name__ == "__main__":
    unittest.main()
