"""Trust only receipt-bound shipped project hooks through native Codex config RPCs.

No synthesized hook hashes, enabled changes, project trust grants, threads or turns.
"""
from __future__ import annotations
import collections
from dataclasses import dataclass
import json
import os
from pathlib import Path
import selectors
import shlex
import signal
import subprocess
import sys
import time
import install

MAX_FRAME = 2 * 1024 * 1024
MAX_TOTAL = 8 * 1024 * 1024
MAX_FRAMES = 128
TIMEOUT = 20
METHODS = frozenset(("initialize", "hooks/list", "config/read", "config/batchWrite"))

class TrustError(Exception):
    """Static, non-secret reason; config/response payloads never enter diagnostics."""


def _json(path, *, with_digest=False):
    raw = install.read_optional(Path(path))
    if raw is None:
        raise TrustError("receipt_or_configuration_missing")
    def pairs(values):
        result = {}
        for key, value in values:
            if key in result:
                raise TrustError("duplicate_configuration_key")
            result[key] = value
        return result
    try:
        value = json.loads(raw, object_pairs_hook=pairs)
        return (value, install.digest(raw)) if with_digest else value
    except (ValueError, UnicodeError):
        raise TrustError("invalid_receipt_or_configuration") from None


@dataclass(frozen=True)
class ReviewedHooks:
    signatures: tuple
    source_sha256: str


def reviewed_definitions(root, service_path, bundle_root=None):
    """Bind bytes AND reviewed configuration, not just editable receipt claims."""
    root, service_path = Path(root), Path(service_path)
    prefix = service_path.parent.parent
    if prefix.is_symlink() or service_path != prefix / "config/service.json":
        raise TrustError("invalid_installed_prefix")
    receipt = _json(prefix / "receipt.json")
    if not isinstance(receipt, dict) or receipt.get("schema") != install.SCHEMA:
        raise TrustError("invalid_install_receipt")
    plan = receipt.get("plan")
    try:
        planned_root, planned_prefix = install.validate_plan(plan)
    except (ValueError, KeyError, TypeError):
        raise TrustError("invalid_install_receipt") from None
    if (planned_root != root or planned_prefix != prefix
            or plan.get("config_revision") not in install.SUPPORTED_APPLY_REVISIONS
            or Path(plan["python"]).resolve() != Path(sys.executable).resolve()):
        raise TrustError("receipt_scope_or_interpreter_changed")
    try:
        install.revalidate_existing_service(plan)
    except (ValueError, OSError, KeyError, TypeError):
        raise TrustError("existing_service_configuration_or_binary_changed") from None
    originals = {}
    for name in install.CONFIG_NAMES:
        expected = plan["expected_configs"][name]
        raw = None if expected is None else install.read_optional(prefix / "backups" / name)
        if expected is not None and (raw is None or install.digest(raw) != expected):
            raise TrustError("installer_backup_changed")
        originals[name] = raw
    if receipt.get("installed_configs") != install._output_hashes(install.config_outputs(plan, originals)):
        raise TrustError("receipt_output_claim_changed")
    bundle = Path(bundle_root or Path(__file__).parent)
    rows = {row["destination"]:row for row in plan["files"]}
    for name in install.PROGRAMS:
        shipped = install.read_optional(bundle / name)
        installed = install.read_optional(prefix / "lib" / name)
        if (shipped is None or installed != shipped or rows.get("lib/" + name, {}).get("sha256") != install.digest(shipped)):
            raise TrustError("installed_runtime_changed")
    expected_service, expected_hook = install.runtime_configs(plan)
    if _json(service_path) != expected_service:
        raise TrustError("installed_service_configuration_changed")
    actual_hook = _json(prefix / "config/hooks.json")
    if expected_hook.get("recording_mode") == "automatic" and actual_hook.get("recording_mode") == "off":
        expected_hook["recording_mode"] = "off"  # explicit safe narrowing supported by init
    if actual_hook != expected_hook:
        raise TrustError("installed_hook_configuration_changed")
    command = shlex.join([plan["python"], str(prefix / "lib/hooks.py"), "--config", str(prefix / "config/hooks.json")])
    definitions = install.hook_handlers(command, async_mode=plan["recall_mode"] == "async",
        recording=plan["recording_mode"] == "automatic", revision=plan["config_revision"], recall_mode=plan["recall_mode"])
    signatures = []
    expected_groups = [(event, group) for event, groups in definitions.items() for group in groups]
    for event, groups in definitions.items():
        for group in groups:
            for handler in group["hooks"]:
                signatures.append((event[0].lower() + event[1:], handler["command"],
                    handler.get("async", False), group.get("matcher"), handler.get("timeout"),
                    handler.get("additionalContextLimit"), handler.get("statusMessage")))
    # Native hooks/list is authoritative for keys/hash/trust. Bind filesystem
    # definitions too, so omissions/inactive project layers do not become success.
    actual_definitions, source_sha256 = _json(root / ".codex/hooks.json", with_digest=True)
    commands = {signature[1] for signature in signatures}
    actual = []
    for event, groups in actual_definitions.get("hooks", {}).items():
        if not isinstance(groups, list):
            raise TrustError("project_hook_inventory_changed")
        for group in groups:
            for handler in group.get("hooks", []):
                if handler.get("command") in commands:
                    if (event, group) not in expected_groups:
                        raise TrustError("project_hook_definition_changed")
                    actual.append((event[0].lower() + event[1:], handler["command"],
                        handler.get("async", False), group.get("matcher"), handler.get("timeout"),
                        handler.get("additionalContextLimit"), handler.get("statusMessage")))
    if collections.Counter(actual) != collections.Counter(signatures):
        raise TrustError("project_hook_inventory_changed")
    return ReviewedHooks(tuple(signatures), source_sha256)


class AppServer:
    """Finite configuration-only NDJSON connection; no generic model/tool API."""
    def __init__(self, codex, root):
        self.deadline = time.monotonic() + TIMEOUT
        self.process = subprocess.Popen([str(codex), "app-server", "--listen", "stdio://"], cwd=root,
            stdin=subprocess.PIPE, stdout=subprocess.PIPE, stderr=subprocess.DEVNULL, start_new_session=True)
        self.selector = selectors.DefaultSelector()
        self.selector.register(self.process.stdout, selectors.EVENT_READ)
        os.set_blocking(self.process.stdout.fileno(), False)
        os.set_blocking(self.process.stdin.fileno(), False)
        self.buffer = b""
        self.total = self.frames = self.ident = 0

    def _send(self, message):
        raw = (json.dumps(message, separators=(",", ":")) + "\n").encode()
        if len(raw) > MAX_FRAME:
            raise TrustError("request_limit")
        with selectors.DefaultSelector() as selector:
            selector.register(self.process.stdin, selectors.EVENT_WRITE)
            while raw:
                remaining = self.deadline - time.monotonic()
                if remaining <= 0 or not selector.select(remaining):
                    raise TrustError("app_server_timeout")
                count = os.write(self.process.stdin.fileno(), raw)
                if count <= 0:
                    raise TrustError("app_server_closed")
                raw = raw[count:]

    def _next(self):
        while b"\n" not in self.buffer:
            if len(self.buffer) > MAX_FRAME:
                raise TrustError("response_frame_limit")
            remaining = self.deadline - time.monotonic()
            if remaining <= 0 or not self.selector.select(remaining):
                raise TrustError("app_server_timeout")
            block = os.read(self.process.stdout.fileno(), 65536)
            if not block:
                raise TrustError("app_server_closed")
            self.total += len(block)
            if self.total > MAX_TOTAL:
                raise TrustError("response_total_limit")
            self.buffer += block
        raw, self.buffer = self.buffer.split(b"\n", 1)
        self.frames += 1
        if len(raw) > MAX_FRAME or self.frames > MAX_FRAMES:
            raise TrustError("response_limit")
        try:
            value = json.loads(raw)
        except (ValueError, UnicodeError):
            raise TrustError("invalid_app_server_frame") from None
        if not isinstance(value, dict):
            raise TrustError("invalid_app_server_frame")
        return value

    def request(self, method, params):
        if method not in METHODS:
            raise TrustError("method_not_authorized")
        self.ident += 1
        self._send({"id":self.ident, "method":method, "params":params})
        while True:
            message = self._next()
            if message.get("id") != self.ident:
                # Notifications may announce config/account state. Server requests
                # are not authorized and are never answered with an approval.
                if "id" in message:
                    raise TrustError("unexpected_app_server_request")
                continue
            if "error" in message:
                error = message["error"]
                data = error.get("data") if isinstance(error, dict) else None
                reason = "config_version_conflict" if isinstance(data, dict) and data.get("config_write_error_code") == "configVersionConflict" else "native_rpc_refused"
                raise TrustError(reason)
            if not isinstance(message.get("result"), dict):
                raise TrustError("invalid_app_server_result")
            return message["result"]

    def initialize(self):
        self.request("initialize", {"clientInfo":{"name":"mneme_hook_trust", "version":"1"},
            "capabilities":{"experimentalApi":True, "requestAttestation":False}})
        self._send({"method":"initialized"})

    def close(self):
        self.selector.close()
        if self.process.poll() is None:
            self.process.terminate()
            try:
                self.process.wait(timeout=2)
            except subprocess.TimeoutExpired:
                try:
                    os.killpg(self.process.pid, signal.SIGKILL)
                except ProcessLookupError:
                    pass
                self.process.wait(timeout=2)
        for pipe in (self.process.stdin, self.process.stdout):
            pipe.close()


def scoped_hooks(response, root, signatures):
    entries = response.get("data")
    if not isinstance(entries, list) or len(entries) != 1 or entries[0].get("cwd") != str(root):
        raise TrustError("native_hook_scope_mismatch")
    entry = entries[0]
    if entry.get("errors") or not isinstance(entry.get("hooks"), list):
        raise TrustError("native_hook_inventory_incomplete")
    source = str(root / ".codex/hooks.json")
    commands = {signature[1] for signature in signatures}
    selected, seen, actual = [], set(), []
    for hook in entry["hooks"]:
        if (not isinstance(hook, dict) or hook.get("sourcePath") != source
                or hook.get("command") not in commands):
            continue
        signature = (hook.get("eventName"), hook.get("command"), hook.get("async"),
            hook.get("matcher"), hook.get("timeoutSec"), hook.get("additionalContextLimit"), hook.get("statusMessage"))
        if (hook.get("source") != "project" or hook.get("handlerType") != "command"
                or hook.get("isManaged") is not False or hook.get("pluginId") is not None
                or type(hook.get("enabled")) is not bool or type(hook.get("async")) is not bool
                or (hook.get("timeoutSec") is not None and type(hook["timeoutSec"]) is not int)
                or (hook.get("additionalContextLimit") is not None and type(hook["additionalContextLimit"]) is not int)):
            raise TrustError("native_hook_authority_mismatch")
        key, current = hook.get("key"), hook.get("currentHash")
        if (not isinstance(key, str) or not key or len(key.encode()) > 8192 or key in seen
                or not isinstance(current, str) or not current or len(current) > 256
                or hook.get("trustStatus") not in ("trusted", "untrusted", "modified")):
            raise TrustError("native_hook_identity_invalid")
        seen.add(key)
        actual.append(signature)
        selected.append(hook)
    if collections.Counter(actual) != collections.Counter(signatures):
        raise TrustError("native_hook_inventory_mismatch")
    return selected


def trust_project_hooks(codex, root, service_path, *, client_factory=AppServer, bundle_root=None):
    """Return pending instead of undoing completed project setup after trust failure."""
    result = {"status":"pending", "trusted":0, "disabled_preserved":0}
    client = None
    try:
        reviewed = reviewed_definitions(root, service_path, bundle_root)
        signatures = reviewed.signatures
        client = client_factory(codex, root)
        client.initialize()
        listed = scoped_hooks(client.request("hooks/list", {"cwds":[str(root)]}), root, signatures)
        needs_review = any(hook["enabled"] and hook["trustStatus"] != "trusted" for hook in listed)
        edits = []
        if needs_review:
            config = client.request("config/read", {"includeLayers":True, "cwd":str(root)})
            layers = config.get("layers")
            users = [] if not isinstance(layers, list) else [layer for layer in layers
                if isinstance(layer, dict) and isinstance(layer.get("name"), dict) and layer["name"].get("type") == "user"]
            expected_file = Path(os.environ.get("CODEX_HOME", str(Path.home() / ".codex"))).resolve() / "config.toml"
            if (len(users) != 1 or users[0]["name"].get("file") != str(expected_file)
                    or users[0].get("disabledReason") is not None
                    or not isinstance(users[0].get("version"), str) or not users[0]["version"]):
                raise TrustError("native_user_config_version_unavailable")
            # This authoritative enabled/hash inventory is AFTER config/read.
            # Its user-config version is the CAS fence: a disable during/after
            # listing conflicts instead of trusting a now-disabled hook.
            listed = scoped_hooks(client.request("hooks/list", {"cwds":[str(root)]}), root, signatures)
            edits = [{"keyPath":"hooks.state." + json.dumps(hook["key"]) + ".trusted_hash",
                      "value":hook["currentHash"], "mergeStrategy":"replace"}
                     for hook in listed if hook["enabled"] and hook["trustStatus"] != "trusted"]
            if edits:
                # Pin source bytes plus packaged runtime/config immediately
                # before the native compare-and-swap write.
                if reviewed_definitions(root, service_path, bundle_root) != reviewed:
                    raise TrustError("installed_source_changed_during_review")
                written = client.request("config/batchWrite", {"edits":edits, "filePath":str(expected_file),
                    "expectedVersion":users[0]["version"], "reloadUserConfig":False})
                if written.get("status") != "ok" or written.get("filePath") != str(expected_file):
                    raise TrustError("native_trust_write_unconfirmed")
        enabled = [hook for hook in listed if hook["enabled"]]
        result["disabled_preserved"] = len(listed) - len(enabled)
        after = scoped_hooks(client.request("hooks/list", {"cwds":[str(root)]}), root, signatures)
        before = {hook["key"]:hook for hook in listed}
        for hook in after:
            previous = before.get(hook["key"])
            if previous is None or (hook["currentHash"], hook["enabled"]) != (previous["currentHash"], previous["enabled"]):
                raise TrustError("hook_changed_during_review")
            if hook["enabled"] and hook["trustStatus"] != "trusted":
                raise TrustError("native_trust_readback_pending")
        if reviewed_definitions(root, service_path, bundle_root) != reviewed:
            raise TrustError("installed_runtime_changed_during_review")
        result.update(status="trusted", trusted=len(enabled), changed=len(edits),
                      authority="exact shipped project hooks only; disabled/unrelated hooks unchanged")
    except TrustError as error:
        result["reason"] = str(error)
    except Exception:
        result["reason"] = "hook_trust_unavailable"  # never expose native/config payloads
    finally:
        if client is not None:
            try:
                client.close()
            except Exception:
                result.update(status="pending", reason="app_server_cleanup_failed")
    return result
