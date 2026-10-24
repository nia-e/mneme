"""Exact-authority matrix and finite fake transport; no user trust/provider writes."""
import copy
import base64
import json
import os
from pathlib import Path
import sys
import tempfile
import subprocess
import time
import unittest
from unittest.mock import patch
import hook_trust as trust
import install
import host_files
import host_platform
from host_platform import executable_name

class FakeServer:
    def __init__(self, hooks, user_file):
        self.hooks = hooks
        self.user_file = user_file
        self.calls = []
        self.closed = False
        self.before_config = None
        self.conflict = False
        self.stale_readback = False
        self.wrote = False
    def initialize(self):
        self.calls.append(("initialize", {}))
    def request(self, method, params):
        self.calls.append((method, copy.deepcopy(params)))
        if method == "hooks/list":
            hooks = copy.deepcopy(self.hooks)
            if self.wrote and self.stale_readback:
                hooks[0]["currentHash"] = "sha256:changed-after-write"
            return {"data":[{"cwd":self.root, "hooks":hooks, "warnings":[], "errors":[]}]}
        if method == "config/read":
            if self.before_config:
                self.before_config()
            return {"layers":[{"name":{"type":"user", "file":str(self.user_file), "profile":None}, "version":"native-version"}]}
        if method == "config/batchWrite":
            if self.conflict:
                raise trust.TrustError("config_version_conflict")
            self.wrote = True
            for edit in params["edits"]:
                key = json.loads(edit["keyPath"][len("hooks.state."):-len(".trusted_hash")])
                next(hook for hook in self.hooks if hook["key"] == key)["trustStatus"] = "trusted"
            return {"status":"ok", "filePath":str(self.user_file), "version":"native-new-version"}
        raise AssertionError(method)
    def close(self):
        self.closed = True

class HookTrustTests(unittest.TestCase):
    def setUp(self):
        self.temp = tempfile.TemporaryDirectory()
        self.addCleanup(self.temp.cleanup)
        self.base = Path(self.temp.name).resolve()
        host_files.set_private(self.base, directory=True)
        self.root = self.base / "project"
        self.root.mkdir()
        self.prefix = self.root / ".mneme/runtime"
        self.codex_home = self.base / "codex-home"
        self.codex_home.mkdir()
        self.addCleanup(patch.stopall)
        patch.dict(os.environ, {"HOME":str(self.base), "CODEX_HOME":str(self.codex_home)}).start()
        self.binaries = self.base / "bin"
        self.binaries.mkdir()
        for name in ("mnemed", "mneme-mcp", "codex"):
            path = self.binaries / executable_name(name)
            path.write_text("#!/bin/sh\nexit 1\n")
            path.chmod(0o700)
        plan = install.prepare(self.root, self.prefix, self.binaries / executable_name("mnemed"), self.binaries / executable_name("mneme-mcp"), 19871,
            recall_mode="async", reader_model=install.READER_MODELS[0], reader_codex=self.binaries / executable_name("codex"), recording_mode="automatic")
        install.apply(plan)
        self.service = self.prefix / "config/service.json"
        self.reviewed = trust.reviewed_definitions(self.root, self.service)
        hooks = []
        for index, signature in enumerate(self.reviewed.signatures):
            event, command, asynchronous, matcher, timeout, limit, status = signature
            hooks.append({"key":f'opaque.project.key."{index}"', "eventName":event, "handlerType":"command", "command":command,
                "async":asynchronous, "matcher":matcher, "timeoutSec":timeout, "additionalContextLimit":limit, "statusMessage":status,
                "sourcePath":str(self.root / ".codex/hooks.json"), "source":"project", "pluginId":None,
                "enabled":True, "isManaged":False, "currentHash":f"sha256:native-{index}", "trustStatus":"untrusted"})
        self.client = FakeServer(hooks, self.codex_home / "config.toml")
        self.client.root = str(self.root)
    def run_trust(self):
        return trust.trust_project_hooks(self.binaries / executable_name("codex"), self.root, self.service, client_factory=lambda *_:self.client)
    def writes(self):
        return [params for method, params in self.client.calls if method == "config/batchWrite"]

    def install_existing_owner(self, revision=None):
        owner_path = self.base / "old-service.json"
        owner_path.write_bytes(install.json_bytes({
            "binary": str(self.binaries / executable_name("mneme-mcp")),
            "project_db": str(self.root / ".mneme/codex-memory.db"),
            "working_directory": str(self.root), "port": 19871,
            "state_dir": str(self.base / "retained-owner-state")}))
        install.uninstall(self.prefix / "receipt.json")
        self.prefix = self.root / ".mneme/new-hook-runtime"
        plan = install.prepare(self.root, self.prefix, self.binaries / executable_name("mnemed"), self.binaries / executable_name("mneme-mcp"), 19871,
            recall_mode="async", reader_model=install.READER_MODELS[0], reader_codex=self.binaries / executable_name("codex"),
            recording_mode="automatic", existing_service_config=owner_path)
        if revision is not None:
            plan["config_revision"] = revision
            plan["python"] = sys.executable
            plan["plan_sha256"] = install.plan_hash(plan)
        install.apply(plan)
        self.service = self.prefix / "config/service.json"
        reviewed = trust.reviewed_definitions(self.root, self.service)
        for hook, signature in zip(self.client.hooks, reviewed.signatures):
            hook["command"] = signature[1]
        return owner_path

    def test_dynamic_interpreter_receipt_survives_runtime_path_change(self):
        receipt = json.loads((self.prefix / "receipt.json").read_text())
        self.assertEqual(receipt["plan"]["config_revision"], install.CONFIG_REVISION)
        self.assertEqual(receipt["plan"]["python"], "python3")
        with patch.object(sys, "executable", str(self.base / "replaced-python")):
            self.assertEqual(trust.reviewed_definitions(self.root, self.service), self.reviewed)
        self.install_existing_owner()
        owner_receipt = json.loads((self.prefix / "receipt.json").read_text())
        self.assertEqual(owner_receipt["plan"]["config_revision"], install.EXISTING_SERVICE_REVISION)
        self.assertEqual(owner_receipt["plan"]["python"], "python3")
        with patch.object(sys, "executable", str(self.base / "another-python")):
            trust.reviewed_definitions(self.root, self.service)

    def test_revision38_receipt_retains_interpreter_scope_check(self):
        root = self.base / "legacy-project"
        root.mkdir()
        prefix = root / ".mneme/runtime"
        plan = install.prepare(root, prefix, self.binaries / executable_name("mnemed"),
            self.binaries / executable_name("mneme-mcp"), 19872)
        plan.update(config_revision=38, python=sys.executable)
        plan["plan_sha256"] = install.plan_hash(plan)
        install.apply(plan)
        service = prefix / "config/service.json"
        reviewed = trust.reviewed_definitions(root, service)
        self.assertIn(sys.executable, reviewed.signatures[0][1])
        with patch.object(sys, "executable", str(self.base / "another-python")), \
             self.assertRaisesRegex(trust.TrustError, "receipt_scope_or_interpreter_changed"):
            trust.reviewed_definitions(root, service)

    def test_revision39_owner_receipt_preserves_absolute_interpreter_and_commands(self):
        self.install_existing_owner(revision=39)
        receipt = json.loads((self.prefix / "receipt.json").read_text())
        plan = receipt["plan"]
        self.assertEqual(plan["python"], sys.executable)
        argv = [sys.executable, str(self.prefix / "lib/hooks.py"), "--config", str(self.prefix / "config/hooks.json")]
        expected = host_platform.hook_command(argv, windows=install.plan_platform(plan) == "windows")
        self.assertEqual(install.planned_hook_handlers(plan)["SessionStart"][0]["hooks"][0]["command"], expected)
        trust.reviewed_definitions(self.root, self.service)
        with patch.object(sys, "executable", str(self.base / "another-python")), \
             self.assertRaisesRegex(trust.TrustError, "receipt_scope_or_interpreter_changed"):
            trust.reviewed_definitions(self.root, self.service)

    def test_windows_encoded_background_is_installed_and_trusted_as_exact_full_argv(self):
        # Exercise the Windows renderer on every host, while retaining real
        # receipt/config hashing and native trust CAS request validation.
        root = self.base / "encoded %!& café ' project"
        root.mkdir()
        prefix = root / ".mneme/runtime"
        interpreter = self.binaries / "fixture python.exe"
        interpreter.write_bytes(b"not executed: receipt and command rendering fixture")
        interpreter.chmod(0o700)
        with patch.object(install, "HOST_PLATFORM", "windows"), patch.object(sys, "executable", str(interpreter)):
            plan = install.prepare(root, prefix,
                self.binaries / executable_name("mnemed"), self.binaries / executable_name("mneme-mcp"), 19872,
                recall_mode="async", reader_model=install.READER_MODELS[0],
                reader_codex=self.binaries / executable_name("codex"), recording_mode="automatic")
            plan["python"] = str(interpreter)
            plan["plan_sha256"] = install.plan_hash(plan)
            install.apply(plan)
            argv = [str(interpreter), str(prefix / "lib/hooks.py"), "--config", str(prefix / "config/hooks.json")]
            front = host_platform.hook_command(argv, windows=True)
            background = host_platform.hook_command([*argv, "--reader-background"], windows=True)
            self.assertNotEqual(background, front + " --reader-background")
            script = base64.b64decode(background.rsplit(" ", 1)[1], validate=True).decode("utf-16-le")
            expected_args = subprocess.list2cmdline([*argv[1:], "--reader-background"]).replace("'", "''")
            self.assertIn("$p.Arguments='" + expected_args + "';", script)
            self.assertNotIn("--reader-background", background)
            source = root / ".codex/hooks.json"
            definitions = json.loads(source.read_bytes())
            native = []
            for event, groups in definitions["hooks"].items():
                for group in groups:
                    for handler in group["hooks"]:
                        self.assertEqual(handler["command"], background if handler.get("async") else front)
                        row = dict(self.client.hooks[0], key="exact-encoded-" + str(len(native)),
                            eventName=event[0].lower() + event[1:], command=handler["command"],
                            matcher=group.get("matcher"), timeoutSec=handler.get("timeout"),
                            additionalContextLimit=handler.get("additionalContextLimit"),
                            statusMessage=handler.get("statusMessage"), sourcePath=str(source),
                            currentHash="sha256:native-encoded-" + str(len(native)), trustStatus="untrusted")
                        row["async"] = handler.get("async", False)
                        native.append(row)
            self.assertEqual(len(native), 8)
            self.assertEqual(sum(row["async"] for row in native), 2)
            client = FakeServer(native, self.codex_home / "config.toml")
            client.root = str(root)
            result = trust.trust_project_hooks(self.binaries / executable_name("codex"), root,
                prefix / "config/service.json", client_factory=lambda *_: client)
            self.assertEqual(result["status"], "trusted")
            self.assertEqual(result["trusted"], 8)
            writes = [params for method, params in client.calls if method == "config/batchWrite"]
            self.assertEqual(len(writes), 1)
            self.assertEqual(writes[0]["edits"], [
                {"keyPath": "hooks.state." + json.dumps(row["key"]) + ".trusted_hash",
                 "value": row["currentHash"], "mergeStrategy": "replace"} for row in native])
            # The old broken form is not an equivalent command and must not
            # acquire trust, even if every other installed byte is intact.
            definitions["hooks"]["UserPromptSubmit"][1]["hooks"][0]["command"] = front + " --reader-background"
            source.write_bytes(install.json_bytes(definitions))
            with patch.object(trust, "AppServer") as contacted:
                result = trust.trust_project_hooks(self.binaries / executable_name("codex"), root,
                    prefix / "config/service.json", client_factory=contacted)
                self.assertEqual(result["status"], "pending")
                self.assertEqual(result["reason"], "project_hook_inventory_changed")
                contacted.assert_not_called()

    def test_explicit_existing_owner_trusts_only_exact_fresh_hooks_without_owner_contact(self):
        self.install_existing_owner()
        with patch("service._probe") as probe, patch("service.subprocess.Popen") as spawn:
            self.assertEqual(self.run_trust()["status"], "trusted")
            probe.assert_not_called()
            spawn.assert_not_called()
        self.assertEqual(len(self.writes()[0]["edits"]), 8)

    def test_legacy_owner_config_and_binary_drift_denied_before_native_contact(self):
        owner = self.install_existing_owner(revision=39)
        original = owner.read_bytes()
        for target in (owner, self.binaries / executable_name("mneme-mcp")):
            with self.subTest(target=target):
                target.write_bytes(b"changed\n")
                result = self.run_trust()
                self.assertEqual(result["status"], "pending")
                self.assertEqual(result["reason"], "existing_service_configuration_or_binary_changed")
                self.assertFalse(self.client.calls)
                self.assertFalse(self.writes())
                owner.write_bytes(original)

    def test_dynamic_owner_edition_change_does_not_create_an_implicit_trust_pin(self):
        owner = self.install_existing_owner()
        original = owner.read_bytes()
        (self.binaries / executable_name("mneme-mcp")).write_bytes(b"updated external tool\n")
        with patch("service._probe") as probe, patch("service.subprocess.Popen") as spawn:
            self.assertEqual(self.run_trust()["status"], "trusted")
            probe.assert_not_called()
            spawn.assert_not_called()
        self.assertEqual(owner.read_bytes(), original)

    def test_existing_owner_drift_during_native_review_denies_trust_write(self):
        owner = self.install_existing_owner()
        self.client.before_config = lambda: owner.write_bytes(b"changed\n")
        result = self.run_trust()
        self.assertEqual(result["status"], "pending")
        self.assertEqual(result["reason"], "existing_service_configuration_or_binary_changed")
        self.assertFalse(self.writes())

    def test_existing_owner_runtime_must_match_pinned_state_and_binary(self):
        self.install_existing_owner()
        data = json.loads(self.service.read_text())
        data["state_dir"] = str(self.prefix / "run")
        self.service.write_bytes(install.json_bytes(data))
        self.assertEqual(self.run_trust()["reason"], "installed_service_configuration_changed")
        self.assertFalse(self.client.calls)
        self.assertFalse(self.writes())

    def install_registry(self):
        registry = self.base / "stores.json"
        registry.write_bytes(install.json_bytes({"schema": "mneme.stores.v1", "stores": []}))
        install.uninstall(self.prefix / "receipt.json")
        self.prefix = self.root / ".mneme/registry-hook-runtime"
        plan = install.prepare(self.root, self.prefix,
            self.binaries / executable_name("mnemed"), self.binaries / executable_name("mneme-mcp"), 19871,
            recall_mode="async", reader_model=install.READER_MODELS[0],
            reader_codex=self.binaries / executable_name("codex"), recording_mode="automatic",
            store_registry=registry)
        install.apply(plan)
        self.service = self.prefix / "config/service.json"
        reviewed = trust.reviewed_definitions(self.root, self.service)
        native = []
        for index, signature in enumerate(reviewed.signatures):
            event, command, asynchronous, matcher, timeout, limit, status = signature
            row = dict(self.client.hooks[0], key="exact-registry-" + str(index),
                eventName=event, command=command, matcher=matcher,
                timeoutSec=timeout, additionalContextLimit=limit, statusMessage=status,
                currentHash="sha256:native-registry-" + str(index), trustStatus="untrusted")
            row["async"] = asynchronous
            native.append(row)
        self.client.hooks = native
        return registry

    def test_registry_hooks_are_bound_to_reviewed_bytes_without_owner_contact(self):
        registry = self.install_registry()
        with patch("service._probe") as probe, patch("service.subprocess.Popen") as spawn:
            self.assertEqual(self.run_trust()["status"], "trusted")
            probe.assert_not_called()
            spawn.assert_not_called()
        self.assertEqual(len(self.writes()[0]["edits"]), 9)
        # Even a semantically identical rewrite invalidates this install review.
        self.client.calls.clear()
        registry.write_bytes(registry.read_bytes() + b"\n")
        self.assertEqual(self.run_trust()["reason"], "store_registry_configuration_changed")
        self.assertFalse(self.client.calls)
        self.assertFalse(self.writes())

    def test_registry_drift_during_native_review_denies_trust_write(self):
        registry = self.install_registry()
        self.client.before_config = lambda: registry.write_bytes(registry.read_bytes() + b"\n")
        result = self.run_trust()
        self.assertEqual(result["status"], "pending")
        self.assertEqual(result["reason"], "store_registry_configuration_changed")
        self.assertFalse(self.writes())

    def test_only_exact_project_leaves_and_native_hashes_are_trusted(self):
        foreign = copy.deepcopy(self.client.hooks[0])
        foreign.update(key="foreign-user-hook", source="user", sourcePath=str(self.codex_home / "hooks.json"), command="foreign executable")
        self.client.hooks.append(foreign)
        result = self.run_trust()
        self.assertEqual(result["status"], "trusted")
        self.assertEqual(result["trusted"], 8)
        write, = self.writes()
        self.assertEqual(set(write), {"edits", "filePath", "expectedVersion", "reloadUserConfig"})
        self.assertEqual(write["expectedVersion"], "native-version")
        self.assertEqual(write["filePath"], str(self.codex_home / "config.toml"))
        self.assertIs(write["reloadUserConfig"], False)
        for index, edit in enumerate(write["edits"]):
            self.assertEqual(set(edit), {"keyPath", "value", "mergeStrategy"})
            self.assertEqual(edit["keyPath"], "hooks.state." + json.dumps(self.client.hooks[index]["key"]) + ".trusted_hash")
            self.assertEqual(edit["value"], f"sha256:native-{index}")
            self.assertEqual(edit["mergeStrategy"], "replace")
        self.assertEqual(foreign["trustStatus"], "untrusted")
        self.assertTrue(self.client.closed)
    def test_disabled_hooks_remain_disabled_and_untrusted(self):
        self.client.hooks[0]["enabled"] = False
        result = self.run_trust()
        self.assertEqual(result["disabled_preserved"], 1)
        self.assertEqual(result["trusted"], 7)
        self.assertFalse(self.client.hooks[0]["enabled"])
        self.assertEqual(self.client.hooks[0]["trustStatus"], "untrusted")
        self.assertEqual(len(self.writes()[0]["edits"]), 7)
    def test_native_modified_status_uses_current_server_hash(self):
        self.client.hooks[0]["trustStatus"] = "modified"
        self.assertEqual(self.run_trust()["status"], "trusted")
    def test_already_trusted_rerun_performs_no_config_write(self):
        for hook in self.client.hooks:
            hook["trustStatus"] = "trusted"
        self.assertEqual(self.run_trust()["status"], "trusted")
        self.assertFalse(self.writes())
        self.assertFalse(any(method == "config/read" for method, _ in self.client.calls))
    def test_native_capability_matrix_denies_changed_foreign_or_unknown_definitions(self):
        baseline = copy.deepcopy(self.client.hooks)
        for field, value in [("source","user"), ("sourcePath",str(self.codex_home / "hooks.json")),
            ("handlerType","mcpTool"), ("command","foreign executable"), ("isManaged",True),
            ("pluginId","foreign-plugin"), ("async",True), ("matcher","Bash"), ("timeoutSec",999),
            ("additionalContextLimit",99), ("statusMessage","modified"), ("trustStatus","unknown"),
            ("async",0), ("timeoutSec",5.0), ("additionalContextLimit",1600.0)]:
            with self.subTest(field=field):
                self.client.hooks = copy.deepcopy(baseline)
                self.client.hooks[0][field] = value
                self.client.calls.clear()
                self.assertEqual(self.run_trust()["status"], "pending")
                self.assertFalse(self.writes())
    def test_receipt_and_embedded_runtime_authority_matrix(self):
        candidates = [self.prefix / "receipt.json", self.prefix / "lib/hooks.py", self.prefix / "config/hooks.json", self.prefix / "config/service.json"]
        for path in candidates:
            with self.subTest(path=path.name):
                before = path.read_bytes()
                if path.name == "hooks.py":
                    path.write_bytes(before + b"\n# changed\n")
                else:
                    value = json.loads(before)
                    if path.name == "receipt.json":
                        value["installed_configs"]["hooks.json"] = "0" * 64
                    else:
                        value["unreviewed"] = True
                    path.write_text(json.dumps(value))
                self.client.calls.clear()
                self.assertEqual(self.run_trust()["status"], "pending")
                self.assertFalse(self.writes())
                path.write_bytes(before)
        # A forged, internally consistent receipt cannot bless modified bytes.
        runtime = self.prefix / "lib/hooks.py"
        runtime.write_bytes(runtime.read_bytes() + b"\n# forged\n")
        receipt_path = self.prefix / "receipt.json"
        receipt = json.loads(receipt_path.read_text())
        next(row for row in receipt["plan"]["files"] if row["destination"] == "lib/hooks.py")["sha256"] = install.digest(runtime.read_bytes())
        receipt["plan"]["plan_sha256"] = install.plan_hash(receipt["plan"])
        receipt_path.write_text(json.dumps(receipt))
        self.assertEqual(self.run_trust()["reason"], "installed_runtime_changed")
    def test_unexpected_owned_hook_options_refuse_before_rpc(self):
        path = self.root / ".codex/hooks.json"
        value = json.loads(path.read_text())
        value["hooks"]["SessionStart"][0]["hooks"][0]["unexpectedOption"] = "changed"
        path.write_text(json.dumps(value))
        self.assertEqual(self.run_trust()["reason"], "project_hook_definition_changed")
        self.assertFalse(self.client.calls)
    def test_exact_source_bytes_pinned_before_write_preserves_unrelated_changes(self):
        path = self.root / ".codex/hooks.json"
        def change():
            path.write_bytes(path.read_bytes() + b"\n")
        self.client.before_config = change
        self.assertEqual(self.run_trust()["reason"], "installed_source_changed_during_review")
        self.assertFalse(self.writes())
    def test_disable_between_initial_list_and_config_read_is_preserved(self):
        self.client.before_config = lambda:self.client.hooks[0].update(enabled=False)
        result = self.run_trust()
        self.assertEqual(result["status"], "trusted")
        self.assertEqual(result["disabled_preserved"], 1)
        disabled_key = self.client.hooks[0]["key"]
        self.assertTrue(all(json.loads(edit["keyPath"][len("hooks.state."):-len(".trusted_hash")]) != disabled_key
            for edit in self.writes()[0]["edits"]))
        self.assertEqual(self.client.hooks[0]["trustStatus"], "untrusted")
        methods = [method for method, _ in self.client.calls]
        self.assertEqual(methods[:4], ["initialize", "hooks/list", "config/read", "hooks/list"])

    def test_user_config_cas_conflict_is_pending_not_retried(self):
        self.client.conflict = True
        result = self.run_trust()
        self.assertEqual(result["reason"], "config_version_conflict")
        self.assertEqual(len(self.writes()), 1)
        self.assertFalse(self.client.wrote)
    def test_readback_identity_change_is_not_reported_as_success(self):
        self.client.stale_readback = True
        self.assertEqual(self.run_trust()["reason"], "hook_changed_during_review")
    def test_recording_off_safe_narrowing_still_allows_hook_review(self):
        path = self.prefix / "config/hooks.json"
        config = json.loads(path.read_text())
        config["recording_mode"] = "off"
        path.write_text(json.dumps(config))
        self.assertEqual(self.run_trust()["status"], "trusted")
    def test_finite_transport_supports_only_configuration_rpcs(self):
        binary = self.binaries / "fake-codex"
        sentinel = self.base / "inference-would-run"
        binary.write_text(f'''#!{sys.executable}
import json,sys
for line in sys.stdin:
 m=json.loads(line)
 if 'id' not in m: continue
 if m['method'].startswith(('thread/','turn/')):
  open({str(sentinel)!r},'w').write('bad')
 print(json.dumps({{'id':m['id'],'result':{{'ok':True}}}}),flush=True)
''')
        binary.chmod(0o700)
        native_spawn = trust.host_process.spawn
        def fixture_spawn(argv, **kwargs):
            return native_spawn([sys.executable, *argv], **kwargs)
        with patch.object(trust.host_process, "spawn", side_effect=fixture_spawn):
            client = trust.AppServer(binary, self.root)
        try:
            client.initialize()
            self.assertEqual(client.request("hooks/list", {"cwds":[str(self.root)]}), {"ok":True})
            with self.assertRaisesRegex(trust.TrustError, "method_not_authorized"):
                client.request("thread/start", {})
        finally:
            client.close()
        self.assertFalse(sentinel.exists())
        self.assertIsNotNone(client.process.poll())

class AppServerTransportTests(unittest.TestCase):
    def setUp(self):
        self.temp = tempfile.TemporaryDirectory()
        self.addCleanup(self.temp.cleanup)
        self.root = Path(self.temp.name)

    def client(self, body):
        binary = self.root / "fake-codex.py"
        binary.write_text(body)
        native_spawn = trust.host_process.spawn
        with patch.object(trust.host_process, "spawn",
                          side_effect=lambda argv, **kwargs: native_spawn([sys.executable, *argv], **kwargs)):
            client = trust.AppServer(binary, self.root)
        self.addCleanup(client.close)
        return client

    def test_nonreading_child_write_obeys_shared_deadline(self):
        client = self.client("import time; time.sleep(30)")
        started = time.monotonic()
        client.deadline = started + 0.15
        with self.assertRaisesRegex(trust.TrustError, "app_server_timeout"):
            client.request("config/batchWrite", {"body": "x" * (400 * 1024)})
        self.assertLess(time.monotonic() - started, 2)

    def test_partial_frame_timeout_and_frame_cap_are_finite(self):
        for mode, code in (("partial", "app_server_timeout"), ("oversize", "response_frame_limit")):
            with self.subTest(mode=mode):
                body = ("import sys,time; sys.stdin.readline(); "
                        + ("sys.stdout.write('{' + 'x' * 4096);" if mode == "oversize" else "sys.stdout.write('{');")
                        + "sys.stdout.flush();time.sleep(30)")
                client = self.client(body)
                client.deadline = time.monotonic() + 0.5
                with patch.object(trust, "MAX_FRAME", 1024):
                    with self.assertRaisesRegex(trust.TrustError, code):
                        client.request("initialize", {})
                client.close()

    def test_total_frame_bound_and_server_request_never_grant_authority(self):
        for frame, reason in (({'id':999, 'method':'approval/request'}, "unexpected_app_server_request"),
                              ({'method':'noise', 'params':{'body':'x'*100}}, "response_total_limit")):
            with self.subTest(reason=reason):
                body = "import sys;sys.stdin.readline();print(" + repr(json.dumps(frame)) + ",flush=True)"
                client = self.client(body)
                with patch.object(trust, "MAX_TOTAL", 64):
                    with self.assertRaisesRegex(trust.TrustError, reason):
                        client.request("initialize", {})
                client.close()


if __name__ == "__main__":
    unittest.main()
