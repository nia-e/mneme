"""Configured target admission and stale-job fencing; disposable files only."""
import copy
from dataclasses import replace
import json
import os
from pathlib import Path
import subprocess
import sys
import tempfile
import unittest
from unittest.mock import patch

import profile
from store_registry import project_store, resolve
from target_policy import (TargetPolicy, alias_for, policy_for, target_kwargs,
                           target_service_config)


OWNER = "1" * 26
OTHER_OWNER = "2" * 26


class RegistryTargetPolicyTests(unittest.TestCase):
    def setUp(self):
        temporary = tempfile.TemporaryDirectory()
        self.addCleanup(temporary.cleanup)
        self.base = Path(temporary.name).resolve()
        self.cwd = self.base / "workspace"
        self.cwd.mkdir()
        self.registry = self.base / "stores.json"
        self.service = self.base / "company.json"
        self.service_value = {
            "mode": "connect", "url": "http://127.0.0.1:18765/",
            "database_name": "user", "database_path": "/owner/company.db",
        }
        self.write(self.service, self.service_value)
        self.entry = {
            "name": "company", "service_config": str(self.service),
            "db_alias": "user", "database_path": "/owner/company.db",
            "db_id": OWNER, "purpose": "Company-wide engineering experience.",
            "when": {"profiles": ["default", "private", "isolated"]},
            "read": {"enabled": True, "core": True},
            "record": {"enabled": True, "kinds": ["lesson"],
                       "evidence_kinds": ["tool_result"]},
            "maintenance": {"enabled": True},
        }
        self.write_registry(self.entry)
        # Pure selection and target checks must never start or contact an owner.
        for target in ("service.spawn", "service.client_for_service",
                       "mcp_client.McpClient", "socket.create_connection"):
            guard = patch(target, side_effect=AssertionError("owner contacted"))
            guard.start()
            self.addCleanup(guard.stop)

    @staticmethod
    def write(path, value):
        path.write_text(json.dumps(value), encoding="utf-8")

    def write_registry(self, *entries):
        self.write(self.registry, {"schema": "mneme.stores.v1", "stores": list(entries)})

    def write_profile(self, mode="default", *, stores=None, schema="mneme.profile.v2"):
        folder = self.cwd / ".mneme"
        folder.mkdir(exist_ok=True)
        value = {"schema": schema, "mode": mode}
        if stores is not None:
            value["stores"] = stores
        self.write(folder / "profile.json", value)

    def select(self):
        return resolve(self.cwd, registry_path=self.registry)

    def binding(self):
        selection = self.select()
        return selection, TargetPolicy.from_selected(selection.named("company"), selection)

    def config(self, selection, **extra):
        return {"frozen_selection": selection.to_dict(), "selected_store": "company", **extra}

    def test_selected_target_keeps_logical_name_distinct_from_native_alias(self):
        selection, policy = self.binding()
        self.assertEqual((policy.scope, policy.db_alias, policy.db_id),
                         ("configured", "user", OWNER))
        canonical = json.loads(json.dumps(policy.canonical()))
        self.assertEqual(canonical["registry_binding"]["store_name"], "company")
        self.assertEqual(TargetPolicy(**canonical), policy)
        actual, service = policy_for(self.service, self.cwd, policy)
        self.assertEqual(actual, policy)
        self.assertEqual((service.database_name, service.database_path),
                         ("user", "/owner/company.db"))
        self.assertEqual(target_kwargs(self.config(selection)), {"store_target": policy})
        self.assertEqual(target_service_config(self.config(selection)), self.service)
        self.assertEqual(alias_for(self.config(selection)), "user")
        with self.assertRaisesRegex(ValueError, "project store boundary"):
            policy_for(self.service, self.cwd)

    def test_explicit_destination_routes_by_name_and_does_not_need_default(self):
        other_service = self.base / "archive.json"
        self.write(other_service, {**self.service_value, "database_path": "/owner/archive.db"})
        archive = {**self.entry, "name": "archive", "service_config": str(other_service),
                   "database_path": "/owner/archive.db", "db_id": OTHER_OWNER}
        self.write_registry(self.entry, archive)
        selection = self.select()
        expected = TargetPolicy.from_selected(selection.named("archive"), selection)
        for config in (self.config(selection), {"frozen_selection": selection.to_dict()}):
            with self.subTest(config=config):
                self.assertEqual(target_kwargs(config, destination="archive"),
                                 {"store_target": expected})
                self.assertEqual(target_service_config(config, "archive"), other_service)

    def test_missing_or_unselected_destination_never_falls_back_to_service(self):
        selection = self.select()
        for name in (None, "missing", "user", "project"):
            config = {"frozen_selection": selection.to_dict(),
                      "service_config": str(self.service)}
            if name is not None:
                config["selected_store"] = name
            with self.subTest(name=name):
                with self.assertRaises(ValueError):
                    target_kwargs(config)
                with self.assertRaises(ValueError):
                    target_service_config(config)
        config = self.config(selection)
        with self.assertRaises(ValueError):
            target_kwargs(config, destination="missing")
        with self.assertRaises(ValueError):
            target_service_config(config, "missing")

    def test_canonical_output_cannot_mutate_the_frozen_target(self):
        _, policy = self.binding()
        canonical = policy.canonical()
        original = copy.deepcopy(canonical)
        restored = TargetPolicy(**canonical)
        canonical["registry_binding"]["store_name"] = "elsewhere"
        canonical["registry_binding"]["selection"]["mode"] = "off"
        self.assertEqual(policy.canonical(), original)
        self.assertEqual(restored.canonical(), original)
        self.assertEqual(policy_for(self.service, self.cwd, restored)[0], policy)

    def test_canonical_owner_fields_cannot_override_selected_binding(self):
        _, policy = self.binding()
        changes = ({"db_alias": "other"}, {"database_path": "/owner/other.db"},
                   {"db_id": OTHER_OWNER}, {"service_config": str(self.base / "other.json")},
                   {"workspace_root": str(self.base)}, {"workspace_origin": "cwd"},
                   {"excluded_roots": (str(self.base),)})
        for change in changes:
            with self.subTest(change=change), self.assertRaises(ValueError):
                TargetPolicy(**{**policy.canonical(), **change})
        canonical = policy.canonical()
        canonical["registry_binding"]["store_digest"] = "0" * 64
        with self.assertRaises(ValueError):
            TargetPolicy(**canonical)

    def test_from_selected_rejects_forged_or_unchecked_bindings(self):
        selection = self.select()
        selected = selection.named("company")
        for changed in (replace(selected, purpose="Anything goes"),
                        replace(selected, db_id=OTHER_OWNER), selected.to_dict()):
            with self.subTest(changed=changed), self.assertRaises(ValueError):
                TargetPolicy.from_selected(changed, selection)
        with self.assertRaises(ValueError):
            TargetPolicy.from_selected(selected, selection.to_dict())

    def test_configured_policy_cannot_move_to_another_workspace_or_service(self):
        _, policy = self.binding()
        nested = self.cwd / "nested"
        nested.mkdir()
        with self.assertRaisesRegex(ValueError, "workspace changed"):
            policy.validate_workspace(nested)
        with self.assertRaises(ValueError):
            policy_for(self.service, nested, policy)
        with self.assertRaises(ValueError):
            policy_for(self.base / "different.json", self.cwd, policy)

    def test_project_subdirectory_accepts_owner_root_without_rebinding_cwd(self):
        nested = self.cwd / "src"
        nested.mkdir()
        sibling = self.cwd / "other"
        sibling.mkdir()
        service_path = self.base / "project.json"
        self.write(service_path, {**self.service_value, "database_name": "project",
                                  "database_path": "/owner/project.db"})
        project = project_store(service_path, self.cwd, database_path="/owner/project.db",
                                db_id=OTHER_OWNER, read={"enabled": True})
        selection = resolve(nested, registry_path=self.registry, project_binding=project)
        # Reader callers pass the selected cwd; recorder and stewardship callers
        # pass the owning project root. Both describe this same frozen selection.
        for name in ("project", "company"):
            with self.subTest(name=name):
                store = selection.named(name)
                policy = TargetPolicy.from_selected(store, selection)
                self.assertEqual(policy.workspace_root, str(self.cwd))
                self.assertEqual(TargetPolicy(**policy.canonical()), policy)
                for root in (self.cwd, nested):
                    self.assertEqual(policy_for(store.service_config, root, policy)[0], policy)
                policy.validate_workspace(nested)
                with self.assertRaisesRegex(ValueError, "workspace changed"):
                    policy.validate_workspace(sibling)
                with self.assertRaises(ValueError):
                    policy_for(store.service_config, sibling, policy)

    def test_valid_lexical_cwd_alias_accepts_its_canonical_reader_root(self):
        alias = self.base / "workspace-alias"
        try:
            alias.symlink_to(self.cwd, target_is_directory=True)
        except OSError as error:
            self.skipTest(f"directory symlinks unavailable: {error}")
        selection = resolve(alias, registry_path=self.registry)
        policy = TargetPolicy.from_selected(selection.named("company"), selection)
        self.assertEqual(policy.workspace_root, str(self.cwd))
        for spelling in (alias, self.cwd):
            self.assertEqual(policy_for(self.service, spelling, policy)[0], policy)
            policy.validate_workspace(spelling)

    def test_registry_service_and_profile_drift_invalidate_all_target_adapters(self):
        self.write_profile(stores=["company"])
        selection, policy = self.binding()
        canonical = policy.canonical()
        selected = selection.named("company")
        config = self.config(selection)
        profile_path = self.cwd / ".mneme/profile.json"
        original_profile = profile_path.read_text(encoding="utf-8")
        mutations = {
            "purpose": lambda: self.write_registry({**self.entry, "purpose": "A new purpose"}),
            "owner": lambda: self.write_registry({**self.entry, "db_id": OTHER_OWNER}),
            "service": lambda: self.write(self.service, {**self.service_value,
                                                         "url": "http://127.0.0.1:18766/"}),
            "profile": lambda: self.write_profile("private", stores=["company"]),
            "allowlist": lambda: self.write_profile(stores=[]),
        }
        for name, mutate in mutations.items():
            with self.subTest(mutation=name):
                mutate()
                operations = (
                    lambda: TargetPolicy(**canonical),
                    lambda: TargetPolicy.from_selected(selected, selection),
                    lambda: policy_for(self.service, self.cwd, policy),
                    lambda: target_kwargs(config),
                    lambda: target_service_config(config),
                    lambda: profile.permit_service(profile.select_for_cwd(self.cwd), "user",
                        selected.database_path, self.service, selected_store=selected,
                        registry_selection=selection),
                )
                try:
                    for operation in operations:
                        with self.assertRaises(ValueError):
                            operation()
                finally:
                    self.write_registry(self.entry)
                    self.write(self.service, self.service_value)
                    profile_path.write_text(original_profile, encoding="utf-8")

    def test_isolated_and_empty_allowlist_deny_before_service_parsing(self):
        for schema, mode, stores in (("mneme.profile.v1", "isolated", None),
                                     ("mneme.profile.v2", "default", []),
                                     ("mneme.profile.v2", "isolated", [])):
            with self.subTest(schema=schema, mode=mode):
                self.write_profile(mode, stores=stores, schema=schema)
                with patch("store_registry.load_config", side_effect=AssertionError("service parsed")):
                    selection = self.select()
                self.assertEqual(selection.stores, ())
                with self.assertRaises(ValueError):
                    target_kwargs({"frozen_selection": selection.to_dict(), "selected_store": "company"})

    def test_v2_user_service_requires_both_checked_binding_components(self):
        self.write_profile(stores=["company"])
        selection = self.select()
        selected = selection.named("company")
        current_profile = profile.select_for_cwd(self.cwd)
        for kwargs in ({}, {"selected_store": selected}, {"registry_selection": selection}):
            with self.subTest(kwargs=kwargs), self.assertRaises(ValueError):
                profile.permit_service(current_profile, "user", selected.database_path,
                                       self.service, **kwargs)
        profile.permit_service(current_profile, "user", selected.database_path, self.service,
                               selected_store=selected, registry_selection=selection)

    @unittest.skipUnless(hasattr(os, "mkfifo"), "Unix FIFO admission boundary")
    def test_profile_fifo_is_refused_without_waiting_for_a_writer(self):
        folder = self.cwd / ".mneme"
        folder.mkdir()
        os.mkfifo(folder / "profile.json")
        # A child gives the blocking-I/O regression a hard deadline rather than
        # hanging the whole test runner. It imports only the profile parser.
        script = (
            "import profile, sys\n"
            "try:\n"
            "    profile.load_profile(sys.argv[1])\n"
            "except (ValueError, OSError):\n"
            "    pass\n"
            "else:\n"
            "    raise AssertionError('FIFO admitted as a profile')\n"
        )
        result = subprocess.run([sys.executable, "-c", script, str(self.cwd)],
                                cwd=Path(__file__).parent, timeout=3,
                                capture_output=True, text=True, check=False)
        self.assertEqual(result.returncode, 0, result.stderr)

    def test_lexical_nested_project_cannot_escape_through_a_symlink(self):
        nested = self.cwd / "nested"
        (nested / ".git").mkdir(parents=True)
        sibling = self.cwd / "sibling"
        sibling.mkdir()
        alias = nested / "alias"
        self.write_registry()
        project = project_store(self.base / "absent-project.json", self.cwd,
                                database_path="/owner/project.db", read={"enabled": True})
        for destination in (sibling, self.cwd):
            with self.subTest(destination=destination):
                try:
                    alias.symlink_to(destination, target_is_directory=True)
                except OSError as error:
                    self.skipTest(f"directory symlinks unavailable: {error}")
                try:
                    with patch("store_registry.load_config", side_effect=AssertionError("owner parsed")):
                        with self.assertRaisesRegex(ValueError, "nested project"):
                            resolve(alias, registry_path=self.registry, project_binding=project)
                finally:
                    alias.unlink()

    def test_arbitrary_alias_needs_matching_checked_binding_even_when_isolated(self):
        binary, database = self.base / "mnemed", self.base / "company.db"
        binary.write_bytes(b"unused fixture executable")
        binary.chmod(0o700)
        database.write_bytes(b"not opened")
        self.write(self.service, {
            "binary": str(binary), "database_name": "company-memory",
            "database_path": str(database), "port": 18765,
            "state_dir": str(self.base / "owner-state"), "working_directory": str(self.cwd),
        })
        self.write_registry({**self.entry, "db_alias": "company-memory",
                             "database_path": str(database)})
        self.write_profile("isolated", stores=["company"])
        selection, policy = self.binding()
        selected = selection.named("company")
        current_profile = profile.select_for_cwd(self.cwd)
        self.assertEqual(policy_for(self.service, self.cwd, policy)[1].database_name, "company-memory")
        for kwargs in ({}, {"selected_store": selected}, {"registry_selection": selection}):
            with self.subTest(kwargs=kwargs), self.assertRaises(ValueError):
                profile.permit_service(current_profile, "company-memory", database, self.service, **kwargs)
        checked = {"selected_store": selected, "registry_selection": selection}
        profile.permit_service(current_profile, "company-memory", database, self.service, **checked)
        for alias, path, service, selecting in (
            ("other", database, self.service, current_profile),
            ("company-memory", self.base / "other.db", self.service, current_profile),
            ("company-memory", database, self.base / "other.json", current_profile),
            ("company-memory", database, None, current_profile),
            ("company-memory", database, self.service, {**current_profile, "mode": "default"}),
        ):
            with self.subTest(alias=alias, path=path, service=service, selecting=selecting):
                with self.assertRaises(ValueError):
                    profile.permit_service(selecting, alias, path, service, **checked)
        self.assertFalse((self.base / "owner-state").exists())
        self.assertEqual(database.read_bytes(), b"not opened")


if __name__ == "__main__":
    unittest.main()
