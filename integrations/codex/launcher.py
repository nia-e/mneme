"""Bounded Codex stdio MCP front door for one explicitly selected Mneme owner.

The first valid initialize ensures that owner is ready; remote owners remain
connect-only. This process owns only its MCP session, never a database lease.
A failed request is reported once and
never replayed; the next explicit client request may establish a new session.
"""

import argparse
import hashlib
import importlib.util
import json
import os
from pathlib import Path
import re
import sys

from mcp_client import MAX_REQUEST_BYTES, MAX_RESPONSE_BYTES, PROTOCOL_VERSION, McpClient, McpError
from service import _expected_catalog, client_for_service, ensure_ready, load_config
from profile import permit_service, select_for_cwd

ALLOWED_METHODS = frozenset(("initialize", "notifications/initialized", "ping", "tools/list", "tools/call"))
SUPPORTED_VERSIONS = frozenset((PROTOCOL_VERSION, "2025-06-18", "2025-03-26"))
MAX_ERROR_TEXT = 256
DB_ID = re.compile(r"[0-7][0-9A-HJKMNP-TV-Z]{25}\Z", re.IGNORECASE)


class RelayError(Exception):
    pass


def _json_bytes(value):
    try:
        data = json.dumps(value, separators=(",", ":"), ensure_ascii=False,
                          allow_nan=False).encode("utf-8")
    except (TypeError, ValueError) as error:
        raise RelayError("invalid JSON-RPC value") from error
    if len(data) > MAX_REQUEST_BYTES:
        raise RelayError("MCP request exceeds local 128 KiB limit")
    return data


def _parse_frame(raw):
    def no_duplicates(pairs):
        result = {}
        for key, value in pairs:
            if key in result:
                raise ValueError("duplicate JSON key")
            result[key] = value
        return result
    try:
        value = json.loads(raw.decode("utf-8"), object_pairs_hook=no_duplicates)
    except (UnicodeError, ValueError, RecursionError) as error:
        raise RelayError("invalid bounded JSON-RPC frame") from error
    if not isinstance(value, dict) or value.get("jsonrpc") != "2.0":
        raise RelayError("invalid JSON-RPC envelope")
    method = value.get("method")
    if not isinstance(method, str) or not method or len(method) > 128:
        raise RelayError("invalid JSON-RPC method")
    if not isinstance(value.get("params", {}), dict):
        raise RelayError("JSON-RPC params must be an object")
    if "id" in value and (isinstance(value["id"], (dict, list, bool))):
        raise RelayError("invalid JSON-RPC id")
    if method == "notifications/initialized":
        if "id" in value:
            raise RelayError("initialized notification must not have an id")
    elif "id" not in value:
        raise RelayError("MCP request needs an id")
    return value


def _response(ident, *, result=None, error=None):
    value = {"jsonrpc": "2.0", "id": ident}
    value["error" if error is not None else "result"] = error if error is not None else result
    encoded = json.dumps(value, separators=(",", ":"), ensure_ascii=False,
                         allow_nan=False).encode("utf-8")
    if len(encoded) > MAX_RESPONSE_BYTES:
        return _response(ident, error={"code": -32603, "message": "MCP response exceeds local 512 KiB limit"})
    return encoded + b"\n"


def _error(ident, code, message):
    return _response(ident, error={"code": code, "message": message[:MAX_ERROR_TEXT]})


def _owner_scoped(schema):
    """Recognize root or union-branch owner scope; native validates the guard.

    This is routing, not an independent schema validator. The native client
    rejects malformed or action-inapplicable guards rather than retrying without
    one. An old db-only schema or a union advertisement must not silently become
    an unguarded call here: exact-owner registry selection has no legacy downgrade.
    """
    if {"db", "expected_db_id"}.intersection(schema.get("properties", {})):
        return True
    branches = schema.get("oneOf", [])
    return (isinstance(branches, list) and any(isinstance(branch, dict)
            and isinstance(branch.get("properties"), dict)
            and {"db", "expected_db_id"}.intersection(branch["properties"]) for branch in branches))


class Relay:
    def __init__(self, config, *, ensure=ensure_ready, client_factory=None, selection=None, store_name=None):
        self.config = config
        self.ensure = ensure
        self.client_factory = client_factory
        self.client = None
        self.client_initialized = False
        self.selection = selection
        self.store_name = store_name
        self.guarded_tools = frozenset()

    def _selected(self):
        if self.selection is None:
            return None
        from store_registry import revalidate
        selection = revalidate(self.selection, operation="read", store_name=self.store_name)
        selected = selection.named(self.store_name)
        selected.service()  # Do not continue with a changed service config.
        return selected

    def _reset(self):
        client, self.client = self.client, None
        if client is not None:
            client.close()

    def _connect(self):
        selected = self._selected()
        self.ensure(self.config)
        # Readiness may have waited; it must not extend a stale selection.
        if selected is not None:
            self._selected()
        self.client = (self.client_factory(self.config.url,
                                           token_env=self.config.token_env or None, timeout=30.0)
                       if self.client_factory is not None else
                       client_for_service(self.config, timeout=30.0))
        self.client.connect()
        if selected is not None:
            catalog = self.client.call_tool("databases", {})
            if (not _expected_catalog(self.config, catalog)
                    or selected.db_id is not None and catalog[0].get("db_id") != selected.db_id):
                raise RelayError("selected store has an unexpected database catalog")
            # Keep native profile/tool authority intact. Automatic recording
            # grants do not authorize (or forbid) manually requested MCP writes.
            tools = self.client.list_tools()
            if any(not isinstance(tool, dict) or not isinstance(tool.get("name"), str)
                   or not isinstance(tool.get("inputSchema"), dict)
                   or not isinstance(tool["inputSchema"].get("properties", {}), dict) for tool in tools):
                raise RelayError("selected store tool catalog is malformed")
            self.guarded_tools = frozenset(tool["name"] for tool in tools
                                          if _owner_scoped(tool["inputSchema"]))
            self._selected()
        return self.client.connect_result

    def _params(self, method, params, selected):
        if selected is None or method != "tools/call":
            return params
        arguments = params.get("arguments", {})
        if not isinstance(arguments, dict):
            raise RelayError("MCP tool arguments must be an object")
        if "db" in arguments and arguments["db"] != selected.db_alias:
            raise RelayError("MCP database alias differs from selected store")
        if "expected_db_id" in arguments and arguments["expected_db_id"] != selected.db_id:
            raise RelayError("MCP identity pin differs from selected store")
        if params.get("name") in self.guarded_tools and selected.db_id is not None:
            return {**params, "arguments": {**arguments, "db": selected.db_alias,
                                            "expected_db_id": selected.db_id}}
        return params

    def handle(self, message):
        method = message["method"]
        ident = message.get("id")
        if method not in ALLOWED_METHODS:
            return None if ident is None else _error(ident, -32601, "method not found")
        try:
            selected = self._selected()
            if method == "initialize":
                self._reset()
                self.client_initialized = False
                params = message.get("params", {})
                if params.get("protocolVersion") not in SUPPORTED_VERSIONS:
                    raise RelayError("unsupported MCP protocol version")
                if "capabilities" in params and not isinstance(params["capabilities"], dict):
                    raise RelayError("invalid MCP client capabilities")
                if "clientInfo" in params and not isinstance(params["clientInfo"], dict):
                    raise RelayError("invalid MCP client information")
                result = self._connect()
                if not isinstance(result, dict):
                    raise RelayError("native MCP initialize result malformed")
                # The outer stdio session is virtual: native transport may use
                # the latest protocol while this relay retains an older Codex
                # client's supported version and raw tool semantics.
                result = {**result, "protocolVersion": params["protocolVersion"]}
                self.client_initialized = True
            elif not self.client_initialized:
                raise RelayError("initialize before using Mneme MCP")
            elif method == "notifications/initialized":
                return None
            else:
                if self.client is None:
                    self._connect()
                selected = self._selected()
                result = self.client.raw_rpc(method, self._params(method, message.get("params", {}), selected))
            self._selected()
            return None if ident is None else _response(ident, result=result)
        except (McpError, RelayError, ValueError, RuntimeError, OSError) as error:
            self._reset()
            if method == "initialize":
                self.client_initialized = False
            if ident is None:
                return None
            return _error(ident, -32000, str(error) + "; request was not retried")

    def close(self):
        self._reset()


def run_stream(config, source, sink, *, ensure=ensure_ready, client_factory=None,
               selection=None, store_name=None):
    relay = Relay(config, ensure=ensure, client_factory=client_factory,
                  selection=selection, store_name=store_name)
    try:
        while True:
            raw = source.readline(MAX_REQUEST_BYTES + 2)
            if not raw:
                break
            if len(raw) > MAX_REQUEST_BYTES or not raw.endswith(b"\n"):
                # Drain the rest of an oversized frame without retaining it.
                while raw and not raw.endswith(b"\n"):
                    raw = source.readline(4096)
                sink.write(_error(None, -32600, "MCP stdio frame exceeds local 128 KiB limit"))
                sink.flush()
                continue
            try:
                message = _parse_frame(raw)
            except RelayError as error:
                sink.write(_error(None, -32700, str(error)))
                sink.flush()
                continue
            response = relay.handle(message)
            if response is not None:
                sink.write(response)
                sink.flush()
    finally:
        relay.close()


def _enroll_selected_project(config, selection, service_path):
    """Queue one descriptor only after the exact live catalog proves a db_id."""
    if (config.database_name != "project" or selection["mode"] != "default"
            or not selection["configured"] or selection["library_config"] is None):
        return
    marker = Path(service_path).parent / "enrollment.json"
    if not marker.is_file() or marker.is_symlink():
        return
    if marker.stat().st_size > 8192:
        raise ValueError("library enrollment marker exceeds size limit")
    value = json.loads(marker.read_text(encoding="utf-8"))
    if (not isinstance(value, dict) or set(value) != {"mode", "project_root", "helper_sha256"}
            or value["mode"] not in ("none", "fresh", "existing")
            or value["project_root"] != str(selection["root"])):
        raise ValueError("invalid library enrollment marker")
    if value["mode"] == "none":
        return
    helper_path = marker.parent.parent / "lib/library.py"
    if helper_path.is_symlink() or hashlib.sha256(helper_path.read_bytes()).hexdigest() != value["helper_sha256"]:
        raise ValueError("installed library helper does not match reviewed pin")
    with McpClient(config.url, token_env=config.token_env or None, timeout=2.0) as client:
        catalog = client.call_tool("databases", {})
    if not _expected_catalog(config, catalog):
        raise ValueError("project catalog changed before enrollment")
    db_id = catalog[0].get("db_id")
    if not isinstance(db_id, str) or DB_ID.fullmatch(db_id) is None:
        raise ValueError("project catalog has no verified db_id")
    spec = importlib.util.spec_from_file_location("mneme_pinned_library", helper_path)
    if spec is None or spec.loader is None:
        raise ValueError("installed library helper unavailable")
    module = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(module)
    endpoint = {"url": config.url}
    if config.token_env:
        endpoint["token_env"] = config.token_env
    module.enroll_project(selection["library_config"], selection["root"], db_id,
                          database=config.database_name, owner_endpoint=endpoint)


def main(argv=None):
    parser = argparse.ArgumentParser(description=__doc__)
    routing = parser.add_mutually_exclusive_group(required=True)
    routing.add_argument("--service-config", help="existing explicit private Mneme service JSON")
    routing.add_argument("--store-registry", help="explicit named-store registry JSON")
    parser.add_argument("--store", help="read-enabled named store selected for this workspace")
    args = parser.parse_args(argv)
    if args.store_registry:
        if args.store is None:
            parser.error("--store-registry requires --store")
        from store_registry import resolve, revalidate
        selection = resolve(Path.cwd(), registry_path=args.store_registry, mode="read")
        selection = revalidate(selection, operation="read", store_name=args.store)
        selected = selection.named(args.store)
        run_stream(selected.service(), sys.stdin.buffer, sys.stdout.buffer,
                   selection=selection, store_name=args.store)
        return
    if args.store is not None:
        parser.error("--store requires --store-registry")
    selection = select_for_cwd(Path.cwd())
    config = load_config(args.service_config)
    permit_service(selection, config.database_name, config.database_path, args.service_config)
    def ready_and_enroll(chosen):
        ready = ensure_ready(chosen)
        try:
            _enroll_selected_project(chosen, selection, args.service_config)
        except Exception as error:
            # Publication is optional; a broken helper/catalog must not turn
            # a verified local project host into an unavailable MCP server.
            print("Mneme library enrollment pending: %s" % error, file=sys.stderr)
        return ready
    run_stream(config, sys.stdin.buffer, sys.stdout.buffer, ensure=ready_and_enroll)


if __name__ == "__main__":
    main()
