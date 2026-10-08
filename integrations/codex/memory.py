#!/usr/bin/env python3
"""Bounded Codex memory bridge for one explicitly configured user or project store."""

import argparse
import json
import os
from pathlib import Path
import re
import sys

from mcp_client import McpClient, McpError, McpToolError, McpTransportError
from service import ConnectConfig, _expected_catalog, ensure_ready, load_config
from profile import permit_service, select_for_cwd
from touchstone_contract import validate_touchstone_retrieval

MAX_CAPTURE_INPUT = 96 * 1024
MAX_READ_BODY = 96 * 1024
MAX_RECALL_TEXT = 8 * 1024
MAX_CORE_NODES = 32
MAX_CORE_BODY_BYTES = 128 * 1024
DEFAULT_CORE_BODY_BYTES = 8 * 1024
MAX_CORE_RESPONSE_BYTES = 256 * 1024
MAX_EPISODE_RESPONSE_BYTES = 36 * 1024  # Native 32 KiB result plus receipt envelope.
NODE_ID = re.compile(r"[0-7][0-9A-HJKMNP-TV-Z]{25}\Z", re.IGNORECASE)


class BridgeError(Exception):
    def __init__(self, message, *, operation=None, identifier=None, source=None,
                 replayed=None, readback_status=None, retryable=False,
                 episode_id=None, edition_id=None, revision=None,
                 kind=None, origin=None, operation_id=None, attempted=None):
        super().__init__(message)
        self.operation = operation
        self.identifier = identifier
        self.source = source
        self.replayed = replayed
        self.readback_status = readback_status
        self.retryable = retryable
        self.episode_id = episode_id
        self.edition_id = edition_id
        self.revision = revision
        self.kind = kind
        self.origin = origin
        self.operation_id = operation_id
        self.attempted = attempted

    def as_json(self):
        value = {"ok": False, "error": str(self), "retryable": self.retryable}
        if self.operation:
            value["operation"] = self.operation
        if self.identifier:
            value["id"] = self.identifier
        if self.source:
            value["source"] = self.source
        if self.replayed is not None:
            value["replayed"] = self.replayed
        if self.readback_status:
            value["readback_status"] = self.readback_status
        for key in ("episode_id", "edition_id", "revision", "kind", "origin", "operation_id"):
            if getattr(self, key) is not None:
                value[key] = getattr(self, key)
        if self.attempted is not None:
            value["attempted"] = self.attempted
        return value


def _node_id(value):
    if not isinstance(value, str) or not NODE_ID.fullmatch(value):
        raise BridgeError("node id must be a 26-character ULID")
    return value.upper()


def _database_name(value):
    if not isinstance(value, str) or value not in ("user", "project"):
        raise BridgeError("database must be explicitly configured as user or project")
    return value


def _body_limit(value):
    if type(value) is not int or not 1 <= value <= MAX_READ_BODY:
        raise BridgeError("max_body_bytes must be an integer in 1..98304")
    return value


def _operation_input(path, operation):
    """Only byte-bound and decode the input; native Rust validates its meaning."""
    if path == "-":
        raw = sys.stdin.buffer.read(MAX_CAPTURE_INPUT + 1)
    else:
        with Path(path).open("rb") as file:
            raw = file.read(MAX_CAPTURE_INPUT + 1)
    if len(raw) > MAX_CAPTURE_INPUT:
        raise BridgeError("%s input exceeds 96 KiB before MCP request" % operation,
                          operation=operation)
    try:
        value = json.loads(raw)
    except (UnicodeDecodeError, ValueError) as error:
        raise BridgeError("%s input is not JSON" % operation, operation=operation) from error
    return value


def _capture_input(path):
    return _operation_input(path, "capture")


def _tool(client, name, arguments):
    try:
        return client.call_tool(name, arguments)
    except McpToolError as error:
        raise BridgeError("Mneme %s refused: %s" % (name, error), operation=name) from error
    except (McpTransportError, McpError) as error:
        raise BridgeError("Mneme %s unavailable or protocol failed: %s" % (name, error),
                          operation=name, retryable=name == "capture") from error


def _recall_text(text):
    if not isinstance(text, str) or not text.strip() or len(text.encode("utf-8")) > MAX_RECALL_TEXT:
        raise BridgeError("recall text must be 1..8192 UTF-8 bytes", operation="recall")
    return text


def recall(client, text, *, db="project"):
    db = _database_name(db)
    _recall_text(text)
    value = _tool(client, "recall_context", {"db": db, "text": text,
                                            "k": 4, "max_nodes": 8, "depth": 1})
    # v6 remains supported for retained pre-touchstone installed owners. Never
    # interpret a successor facet under that older envelope.
    if (not isinstance(value, dict) or value.get("schema") not in ("mneme.context.v6", "mneme.context.v7")
            or "probationary" in value
            or any(not isinstance(value.get(lane), list)
                   for lane in ("core", "primary", "expansions", "episodes"))
            or any(isinstance(value.get(key), dict) and "probationary" in value[key]
                   for key in ("omitted", "usage"))
            or (isinstance(value.get("retrieval"), dict)
                and isinstance(value["retrieval"].get("lanes"), dict)
                and "probationary" in value["retrieval"]["lanes"])):
        raise BridgeError("Mneme returned unexpected recall context", operation="recall")
    if value["schema"] == "mneme.context.v6" and any(
            "touchstone" in entry for lane in ("core", "primary", "expansions", "episodes")
            for entry in value[lane] if isinstance(entry, dict)):
        raise BridgeError("Mneme returned unexpected recall context", operation="recall")
    if value["schema"] == "mneme.context.v7":
        try:
            validate_touchstone_retrieval(value.get("touchstone_retrieval"))
        except ValueError as error:
            raise BridgeError("Mneme returned unexpected recall context", operation="recall") from error
    return value


def get(client, identifier, *, db="project", max_body_bytes=64 * 1024):
    db = _database_name(db)
    identifier = _node_id(identifier)
    max_body_bytes = _body_limit(max_body_bytes)
    value = _tool(client, "get", {"db": db, "id": identifier,
                                  "body": True, "max_body_bytes": max_body_bytes})
    if not isinstance(value, dict) or value.get("id") != identifier:
        raise BridgeError("Mneme returned unexpected get identity", operation="get", identifier=identifier)
    return value


def _core_response(value, *, max_body_bytes=DEFAULT_CORE_BODY_BYTES):
    """Validate native core bounds without hiding its partial-read metadata."""
    max_body_bytes = _body_limit(max_body_bytes)
    malformed = "Mneme returned malformed or over-budget core context"
    def text_size(text):
        try:
            return len(text.encode("utf-8"))
        except UnicodeError as error:
            raise BridgeError(malformed, operation="core") from error
    if (not isinstance(value, dict) or not isinstance(value.get("nodes"), list)
            or type(value.get("total")) is not int or value["total"] < 0
            or type(value.get("body_bytes_limit")) is not int
            or value["body_bytes_limit"] != MAX_CORE_BODY_BYTES
            or any(type(value.get(key)) is not bool for key in
                   ("truncated", "nodes_truncated", "bodies_truncated"))):
        raise BridgeError(malformed, operation="core")
    nodes = value["nodes"]
    if len(nodes) != min(value["total"], MAX_CORE_NODES):
        raise BridgeError(malformed, operation="core")
    seen, body_bytes, bodies_truncated = set(), 0, False
    for node in nodes:
        if not isinstance(node, dict):
            raise BridgeError(malformed, operation="core")
        try:
            identifier = _node_id(node.get("id"))
        except BridgeError as error:
            raise BridgeError(malformed, operation="core") from error
        summary, tags = node.get("summary"), node.get("tags")
        if (identifier in seen or not isinstance(summary, str)
                or text_size(summary) > 2048
                or type(node.get("summary_truncated")) is not bool
                or not isinstance(tags, list) or len(tags) > 32
                or any(not isinstance(tag, str) or text_size(tag) > 256
                       for tag in tags) or "core" not in tags
                or node.get("status") != "active" or "body" not in node):
            raise BridgeError(malformed, operation="core")
        seen.add(identifier)
        body = node["body"]
        if body is None:
            # Native resolution failure is not an empty body or proof of
            # completeness; retain null for callers to render as unavailable.
            if "body_truncated" in node:
                raise BridgeError(malformed, operation="core")
        elif isinstance(body, str) and type(node.get("body_truncated")) is bool:
            size = text_size(body)
            if size > max_body_bytes:
                raise BridgeError(malformed, operation="core")
            body_bytes += size
            bodies_truncated |= node["body_truncated"]
        else:
            raise BridgeError(malformed, operation="core")
    nodes_truncated = value["total"] > MAX_CORE_NODES
    if (body_bytes > MAX_CORE_BODY_BYTES
            or value["nodes_truncated"] != nodes_truncated
            or value["bodies_truncated"] != bodies_truncated
            or value["truncated"] != (nodes_truncated or bodies_truncated)):
        raise BridgeError(malformed, operation="core")
    try:
        encoded = json.dumps(value, separators=(",", ":"), ensure_ascii=False,
                             allow_nan=False).encode("utf-8")
    except (TypeError, ValueError, RecursionError) as error:
        raise BridgeError(malformed, operation="core") from error
    if len(encoded) > MAX_CORE_RESPONSE_BYTES:
        raise BridgeError(malformed, operation="core")
    return value


def core(client, *, db="project", max_body_bytes=DEFAULT_CORE_BODY_BYTES):
    db = _database_name(db)
    max_body_bytes = _body_limit(max_body_bytes)
    value = _tool(client, "core", {"db": db, "max_body_bytes": max_body_bytes})
    return _core_response(value, max_body_bytes=max_body_bytes)


def capture(client, value, *, db="project"):
    db = _database_name(db)
    try:
        result = client.capture_verified(db, value)
    except McpError as error:
        details = getattr(error, "details", {})
        accepted = details.get("accepted", {}) if isinstance(details, dict) else {}
        raise BridgeError(str(error), operation="capture",
                          source=accepted.get("source", value.get("source")),
                          identifier=accepted.get("id"),
                          replayed=accepted.get("replayed"),
                          readback_status=accepted.get("readback_status", "not_attempted"),
                          retryable=accepted.get("retryable", isinstance(error, McpTransportError))) from error
    if not isinstance(result, dict):
        raise BridgeError("native capture receipt malformed", operation="capture", retryable=True)
    return {"ok": True, "operation": "capture", **result}


def _prepare_save(client, value):
    """Native admission freezes a manual identity before any service startup."""
    try:
        prepared = client.prepare_save(value)
    except McpError as error:
        raise BridgeError(str(error), operation="save", readback_status="not_attempted") from error
    if (not isinstance(prepared, dict) or type(prepared.get("schema")) is not int or prepared["schema"] != 1
            or not isinstance(prepared.get("id"), str)
            or not NODE_ID.fullmatch(prepared["id"])
            or not isinstance(prepared.get("payload"), dict)
            or "db" in prepared["payload"]
            or not isinstance(prepared["payload"].get("source"), dict)):
        raise BridgeError("native save preparation malformed", operation="save",
                          readback_status="not_attempted")
    return prepared


def save(client, value, *, db="project"):
    """One native operation owns note/episode verification; Python only routes."""
    db = _database_name(db)
    try:
        result = client.save_verified(db, value)
    except McpError as error:
        details = getattr(error, "details", {})
        accepted = details.get("accepted", {}) if isinstance(details, dict) else {}
        accepted = accepted if isinstance(accepted, dict) else {}
        attempted = details.get("attempted") if isinstance(details, dict) else None
        raise BridgeError(str(error), operation="save",
                          identifier=accepted.get("id"),
                          source=accepted.get("source", value.get("source") if isinstance(value, dict) else None),
                          replayed=accepted.get("replayed"),
                          readback_status=accepted.get("readback_status", "not_attempted"),
                          retryable=False,
                          attempted=attempted if isinstance(attempted, dict) else None,
                          **{key: accepted.get(key) for key in (
                              "episode_id", "edition_id", "revision", "kind", "origin", "operation_id")}) from error
    if not isinstance(result, dict) or result.get("readback_status") != "verified":
        raise BridgeError("native save has no verified readback", operation="save",
                          readback_status="unverified")
    return {"ok": True, "operation": "save", **result}


def supersede(client, winner, loser, *, db="project"):
    db = _database_name(db)
    winner, loser = _node_id(winner), _node_id(loser)
    if winner == loser:
        raise BridgeError("winner and loser must differ", operation="supersede")
    value = _tool(client, "supersede", {"db": db, "winner": winner, "loser": loser})
    if not isinstance(value, dict) or value.get("db") != db or value.get("ok") is not True:
        raise BridgeError("Mneme returned unexpected supersede result", operation="supersede")
    return {"ok": True, "operation": "supersede", "winner": winner, "loser": loser}


def _prepare_episode(client, value):
    """Native preparation owns action classification, schema and domain bounds."""
    try:
        prepared = client.prepare_episode(value)
    except McpError as error:
        raise BridgeError(str(error), operation="episode", readback_status="not_attempted") from error
    if (not isinstance(prepared, dict) or type(prepared.get("is_mutation")) is not bool
            or not isinstance(prepared.get("action"), str)
            or not isinstance(prepared.get("payload"), dict)
            or "db" in prepared["payload"]):
        raise BridgeError("native episode preparation malformed", operation="episode",
                          readback_status="not_attempted")
    return prepared


def episode(client, prepared, *, db="project"):
    """Forward a native-prepared operation to exactly the configured store."""
    db = _database_name(db)
    if prepared["is_mutation"]:
        try:
            result = client.episode_verified(db, prepared["payload"])
        except McpError as error:
            details = getattr(error, "details", {})
            accepted = details.get("accepted", {}) if isinstance(details, dict) else {}
            accepted = accepted if isinstance(accepted, dict) else {}
            raise BridgeError(str(error), operation="episode",
                              episode_id=accepted.get("episode_id"),
                              edition_id=accepted.get("edition_id"),
                              revision=accepted.get("revision"),
                              source=accepted.get("source"), replayed=accepted.get("replayed"),
                              readback_status=accepted.get("readback_status", "not_attempted"),
                              retryable=False) from error
        if not isinstance(result, dict) or result.get("readback_status") != "verified":
            raise BridgeError("native episode write has no verified readback", operation="episode",
                              readback_status="unverified")
    else:
        result = _tool(client, "episode", {**prepared["payload"], "db": db})
    try:
        encoded = json.dumps(result, separators=(",", ":"), ensure_ascii=False,
                             allow_nan=False).encode("utf-8")
    except (TypeError, ValueError, UnicodeError, RecursionError) as error:
        raise BridgeError("native episode result malformed", operation="episode") from error
    if not isinstance(result, dict) or len(encoded) > MAX_EPISODE_RESPONSE_BYTES:
        raise BridgeError("native episode result malformed or over budget", operation="episode")
    return {"ok": True, "operation": "episode", **result}


def main(argv=None):
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--service-config", required=True, help="explicit single-store service JSON")
    parser.add_argument("--no-start", action="store_true",
                        help="require an already-running service instead of starting it")
    commands = parser.add_subparsers(dest="command", required=True)
    commands.add_parser("core", help="bounded always-loaded core from the configured store")
    commands.add_parser("recall", help="bounded configured-store recall").add_argument("--text", required=True)
    commands.add_parser("get", help="bounded node body readback").add_argument("id")
    commands.add_parser("save", help="save a note or episode with native retry identity").add_argument(
        "--input", required=True, help="SAVE JSON without db; path or - for stdin")
    commands.add_parser("capture", help="compatibility: sourced note capture; prefer save").add_argument("--input", required=True,
                                                                            help="JSON path or - for stdin")
    commands.add_parser("episode", help="configured-store episodic read or verified write").add_argument(
        "--input", required=True, help="episode action JSON without db; path or - for stdin")
    correction = commands.add_parser("supersede", help="explicit winner/loser correction")
    correction.add_argument("--winner", required=True)
    correction.add_argument("--loser", required=True)
    args = parser.parse_args(argv)
    config = None
    try:
        selection = select_for_cwd(Path.cwd())
        config = load_config(args.service_config)
        permit_service(selection, config.database_name, config.database_path, args.service_config)
        if config.token_env and not os.environ.get(config.token_env):
            raise BridgeError("configured bearer token is unavailable", operation=args.command)
        # Native preparation is pure and happens before service start or any MCP connection.
        prepared = (_operation_input(args.input, args.command)
                    if args.command in ("save", "capture", "episode") else None)
        episode_prepared = None
        if args.command in ("save", "capture", "episode"):
            client = McpClient(config.url, token_env=config.token_env or None, timeout=30.0)
            try:
                if args.command == "save":
                    prepared = _prepare_save(client, prepared)["payload"]
                elif args.command == "episode":
                    episode_prepared = _prepare_episode(client, prepared)
                else:
                    client.prepare_capture(prepared)
            except McpError as error:
                raise BridgeError(str(error), operation=args.command, source=prepared.get("source") if isinstance(prepared, dict) else None,
                                  readback_status="not_attempted") from error
            finally:
                client.close()
        if args.command == "recall":
            _recall_text(args.text)
        elif args.command == "get":
            _node_id(args.id)
        elif args.command == "supersede":
            if _node_id(args.winner) == _node_id(args.loser):
                raise BridgeError("winner and loser must differ", operation="supersede")
        if not args.no_start:
            try:
                ensure_ready(config)
            except (OSError, ValueError, RuntimeError) as error:
                raise BridgeError("Mneme service not ready: %s" % error,
                                  operation=args.command,
                                  source=prepared.get("source") if isinstance(prepared, dict) else None,
                                  readback_status="not_attempted" if prepared else None) from error
        with McpClient(config.url, token_env=config.token_env or None, timeout=30.0) as client:
            if config.database_name == "user" or isinstance(config, ConnectConfig):
                # A no-start route must not trust the port alone. Native read
                # envelopes have no database identity: bind them to this exact
                # single-store catalog before any operation, including writes.
                try:
                    if not _expected_catalog(config, _tool(client, "databases", {})):
                        raise BridgeError("catalog does not match the explicit service config")
                except BridgeError as error:
                    raise BridgeError("Mneme %s store catalog verification failed: %s" % (config.database_name, error),
                                      operation=args.command,
                                      source=prepared.get("source") if isinstance(prepared, dict) else None,
                                      readback_status="not_attempted" if prepared else None) from error
            if args.command == "core":
                result = core(client, db=config.database_name)
            elif args.command == "recall":
                result = recall(client, args.text, db=config.database_name)
            elif args.command == "get":
                result = get(client, args.id, db=config.database_name)
            elif args.command == "capture":
                result = capture(client, prepared, db=config.database_name)
            elif args.command == "save":
                result = save(client, prepared, db=config.database_name)
            elif args.command == "episode":
                result = episode(client, episode_prepared, db=config.database_name)
            else:
                result = supersede(client, args.winner, args.loser, db=config.database_name)
        # IDs are store-local. Keep scope on the CLI receipt even when native
        # read envelopes and compatibility helpers have no database field.
        result = {**result, "db": config.database_name}
        print(json.dumps(result, sort_keys=True, separators=(",", ":"), ensure_ascii=False))
        return 0
    except BridgeError as error:
        report = error.as_json()
        if config is not None:
            report["db"] = config.database_name
        print(json.dumps(report, sort_keys=True), file=sys.stderr)
        return 1
    except (McpError, OSError, ValueError) as error:
        report = {"ok": False, "operation": args.command,
                  "error": str(error), "retryable": False}
        if config is not None:
            report["db"] = config.database_name
        print(json.dumps(report, sort_keys=True), file=sys.stderr)
        return 1


if __name__ == "__main__":
    raise SystemExit(main())
