#!/usr/bin/env python3
"""Finite, sequential MCP stdio smoke for an installed Mneme server."""

from __future__ import annotations

import argparse
import json
import os
import selectors
import subprocess
import sys
from pathlib import Path
from typing import Any


PROTOCOL_VERSION = "2025-11-25"
PROFILE_CHOICES = ("read-only", "receipt-grounded", "curator", "operator")
READ_ONLY_TOOLS = {
    "databases",
    "activity",
    "database_control",
    "status",
    "query",
    "recall_context",
    "episode",
    "concern",  # Grouped tool: list is read-only; mutations remain curator.
    "recall",
    "get",
    "list",
    "graph",
    "neighbors",
    "remote_edges",
    "core",
    "contradictions",
    "merges",
    "walk",
}


def parse_args() -> argparse.Namespace:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--binary", default="mneme-mcp")
    parser.add_argument("--user-db", required=True, type=Path)
    parser.add_argument("--project-db", required=True, type=Path)
    parser.add_argument(
        "--capability-profile",
        choices=PROFILE_CHOICES,
        default="receipt-grounded",
    )
    parser.add_argument("--timeout", default=30.0, type=float)
    parser.add_argument("--expect-project-primary-min", default=1, type=int)
    parser.add_argument("--expect-user-primary-min", default=1, type=int)
    parser.add_argument("--expect-project-primary", type=int)
    parser.add_argument("--expect-user-primary", type=int)
    return parser.parse_args()


def send(process: subprocess.Popen[str], message: dict[str, Any]) -> None:
    assert process.stdin is not None
    process.stdin.write(json.dumps(message, separators=(",", ":")) + "\n")
    process.stdin.flush()


def read_response(
    process: subprocess.Popen[str], expected_id: int, timeout: float
) -> dict[str, Any]:
    assert process.stdout is not None
    selector = selectors.DefaultSelector()
    selector.register(process.stdout, selectors.EVENT_READ)
    try:
        if not selector.select(timeout):
            raise RuntimeError(f"timed out waiting for MCP response id {expected_id}")
        line = process.stdout.readline()
    finally:
        selector.close()
    if not line:
        code = process.poll()
        raise RuntimeError(
            f"MCP server closed stdout before response id {expected_id}; exit={code}"
        )
    response = json.loads(line)
    if response.get("jsonrpc") != "2.0":
        raise RuntimeError(
            f"MCP response id {expected_id} has an invalid JSON-RPC version"
        )
    if response.get("id") != expected_id:
        raise RuntimeError(
            f"expected MCP response id {expected_id}, got {response.get('id')!r}"
        )
    if "error" in response:
        raise RuntimeError(f"MCP response id {expected_id} failed: {response['error']}")
    return response


def tool_json(response: dict[str, Any]) -> Any:
    result = response["result"]
    if result.get("isError") is not False:
        raise RuntimeError(f"MCP tool returned an error result: {result!r}")
    content = result.get("content")
    if not isinstance(content, list) or not content:
        raise RuntimeError(f"MCP tool returned no content: {result!r}")
    text = content[0].get("text")
    if not isinstance(text, str):
        raise RuntimeError(f"MCP tool returned non-text content: {content[0]!r}")
    return json.loads(text)


def tool_error_text(response: dict[str, Any]) -> str:
    result = response["result"]
    if result.get("isError") is not True:
        raise RuntimeError(f"expected MCP tool error, got: {result!r}")
    content = result.get("content")
    if not isinstance(content, list) or not content:
        raise RuntimeError(f"MCP tool error returned no content: {result!r}")
    text = content[0].get("text")
    if not isinstance(text, str):
        raise RuntimeError(f"MCP tool error returned non-text content: {content[0]!r}")
    return text


def expected_catalog(profile: str) -> set[str]:
    tools = set(READ_ONLY_TOOLS)
    if profile in {"receipt-grounded", "curator", "operator"}:
        tools.add("reflect")
    if profile in {"curator", "operator"}:
        tools.update({"save", "ingest", "capture", "retag", "link", "contradict"})
    if profile == "operator":
        tools.update(
            {
                "snapshot_create",
                "edit_body",
                "edit_summary",
                "forget",
                "supersede",
                "reconcile",
                "merge",
                "decay",
                "prune",
            }
        )
    # Direct feedback additionally requires the deprecated compatibility flag,
    # which this smoke deliberately never grants.
    return tools


def denied_probe(profile: str) -> tuple[str, dict[str, Any], str]:
    valid_id = "00000000000000000000000000"
    if profile in {"read-only", "receipt-grounded"}:
        return (
            "save",
            {"db": "does-not-exist", "kind": "note", "summary": "capability probe",
             "operation_id": "stdio-capability-probe"},
            "capability profile",
        )
    if profile == "curator":
        return (
            "edit_summary",
            {
                "db": "does-not-exist",
                "expected_db_id": valid_id,
                "id": valid_id,
                "expected_snapshot_sha256": "a" * 64,
                "summary": "capability probe",
            },
            "capability profile",
        )
    return (
        "feedback",
        {
            "db": "does-not-exist",
            "to": valid_id,
            "signal": "relevant",
        },
        "direct feedback compatibility is disabled",
    )


def recall_request(request_id: int, db: str, text: str) -> dict[str, Any]:
    return {
        "jsonrpc": "2.0",
        "id": request_id,
        "method": "tools/call",
        "params": {
            "name": "recall_context",
            "arguments": {
                "db": db,
                "text": text,
                "k": 4,
                "depth": 1,
                "max_nodes": 8,
            },
        },
    }


def context_summary(context: dict[str, Any]) -> dict[str, Any]:
    if context.get("schema") != "mneme.context.v7":
        raise RuntimeError(f"unexpected context schema: {context.get('schema')!r}")
    lanes = {name: context.get(name) for name in ("core", "primary", "expansions", "episodes")}
    retrieval = context.get("retrieval")
    episodic_retrieval = context.get("episodic_retrieval")
    if not all(isinstance(lane, list) for lane in lanes.values()):
        raise RuntimeError("context lanes are not arrays")
    if ("probationary" in context
            or isinstance(context.get("omitted"), dict) and "probationary" in context["omitted"]
            or isinstance(context.get("usage"), dict) and "probationary" in context["usage"]):
        raise RuntimeError("context contains obsolete probationary lane")
    if not isinstance(retrieval, dict) or not isinstance(episodic_retrieval, dict):
        raise RuntimeError("context retrieval metadata is absent")
    retrieval_lanes = retrieval.get("lanes")
    if (not isinstance(retrieval_lanes, dict)
            or set(retrieval_lanes) != {"primary"}
            or not isinstance(retrieval_lanes["primary"], dict)
            or "seed_coverage" not in retrieval_lanes["primary"]):
        raise RuntimeError("context retrieval lanes are not primary-only")
    return {
        "schema": context["schema"],
        **{name: len(cards) for name, cards in lanes.items()},
        "episodic_state": episodic_retrieval.get("state"),
        "presentation_partial": context.get("partial"),
        "retrieval_partial": retrieval.get("partial"),
    }


def require_minimum(label: str, actual: int, minimum: int) -> None:
    if minimum < 0:
        raise RuntimeError(f"{label} minimum must be non-negative")
    if actual < minimum:
        raise RuntimeError(f"{label}: expected at least {minimum}, got {actual}")


def require_exact(label: str, actual: int, expected: int | None) -> None:
    if expected is not None and actual != expected:
        raise RuntimeError(f"{label}: expected exactly {expected}, got {actual}")


def stop_process(process: subprocess.Popen[str]) -> None:
    if process.poll() is not None:
        return
    process.terminate()
    try:
        process.wait(timeout=2)
    except subprocess.TimeoutExpired:
        process.kill()
        process.wait(timeout=2)


def main() -> int:
    args = parse_args()
    command = [
        args.binary,
        "--capability-profile",
        args.capability_profile,
        "--db",
        f"user={args.user_db}",
        "--db",
        f"project={args.project_db}",
    ]
    process = subprocess.Popen(
        command,
        stdin=subprocess.PIPE,
        stdout=subprocess.PIPE,
        stderr=subprocess.PIPE,
        text=True,
        bufsize=1,
        env=os.environ.copy(),
    )
    try:
        send(
            process,
            {
                "jsonrpc": "2.0",
                "id": 1,
                "method": "initialize",
                "params": {
                    "protocolVersion": PROTOCOL_VERSION,
                    "capabilities": {},
                    "clientInfo": {"name": "mneme-smoke", "version": "1"},
                },
            },
        )
        initialize = read_response(process, 1, args.timeout)
        if initialize["result"].get("protocolVersion") != PROTOCOL_VERSION:
            raise RuntimeError("MCP server negotiated an unexpected protocol version")
        if initialize["result"].get("capabilityProfile") != args.capability_profile:
            raise RuntimeError("MCP server reported an unexpected capability profile")

        send(
            process,
            {
                "jsonrpc": "2.0",
                "method": "notifications/initialized",
                "params": {},
            },
        )

        send(
            process,
            {
                "jsonrpc": "2.0",
                "id": 2,
                "method": "tools/list",
                "params": {},
            },
        )
        catalog_response = read_response(process, 2, args.timeout)
        catalog = catalog_response["result"].get("tools")
        if not isinstance(catalog, list) or any(
            not isinstance(tool, dict) or not isinstance(tool.get("name"), str)
            for tool in catalog
        ):
            raise RuntimeError(f"malformed tool catalog: {catalog!r}")
        advertised = {tool["name"] for tool in catalog}
        expected = expected_catalog(args.capability_profile)
        if advertised != expected:
            raise RuntimeError(
                f"unexpected {args.capability_profile} catalog: "
                f"missing={sorted(expected - advertised)!r} "
                f"extra={sorted(advertised - expected)!r}"
            )

        denied_name, denied_arguments, denied_fragment = denied_probe(
            args.capability_profile
        )
        send(
            process,
            {
                "jsonrpc": "2.0",
                "id": 3,
                "method": "tools/call",
                "params": {
                    "name": denied_name,
                    "arguments": denied_arguments,
                },
            },
        )
        denied = tool_error_text(read_response(process, 3, args.timeout))
        if denied_fragment not in denied:
            raise RuntimeError(
                f"denied {denied_name} reached the wrong boundary: {denied!r}"
            )

        send(
            process,
            {
                "jsonrpc": "2.0",
                "id": 4,
                "method": "tools/call",
                "params": {"name": "databases", "arguments": {}},
            },
        )
        databases = tool_json(read_response(process, 4, args.timeout))
        if not isinstance(databases, list) or any(
            not isinstance(item, dict) or not isinstance(item.get("name"), str)
            for item in databases
        ):
            raise RuntimeError(f"malformed database registry: {databases!r}")
        names = sorted(item["name"] for item in databases)
        if names != ["project", "user"]:
            raise RuntimeError(f"unexpected database registry: {names!r}")

        send(
            process,
            recall_request(5, "project", "Mneme tagged retrieval and F7 migration"),
        )
        project_context = tool_json(read_response(process, 5, args.timeout))
        project = context_summary(project_context)

        send(
            process,
            recall_request(6, "user", "user preferences and working style"),
        )
        user_context = tool_json(read_response(process, 6, args.timeout))
        user = context_summary(user_context)

        # This is a fresh, sequential owner: precisely these two successful
        # recalls have prepared results. Polls must not create their own events.
        def read_activity(request_id: int, after: int) -> dict[str, Any]:
            send(process, {
                "jsonrpc": "2.0", "id": request_id, "method": "tools/call",
                "params": {"name": "activity", "arguments": {"after": after, "limit": 64}},
            })
            page = tool_json(read_response(process, request_id, args.timeout))
            if (not isinstance(page, dict) or page.get("schema") != "mneme.activity.v1"
                    or page.get("busy") is not False or page.get("missed") is not False
                    or page.get("has_more") is not False or page.get("dropped") != 0):
                raise RuntimeError(f"unexpected isolated activity page: {page!r}")
            return page

        activity = read_activity(7, 0)
        events = activity.get("events")
        if not isinstance(events, list) or len(events) != 2 or activity.get("next_after") != 2:
            raise RuntimeError(f"expected precisely two recall events: {activity!r}")
        db_ids = {item["name"]: item["db_id"] for item in databases}
        for seq, (event, (db, context)) in enumerate(
            zip(events, (("project", project_context), ("user", user_context))), start=1
        ):
            expected_ids = [card["id"] for lane in ("core", "primary", "expansions", "episodes") for card in context[lane]]
            expected_fields = {"seq", "timestamp_ms", "tool", "db", "db_id", "node_ids",
                               "node_ids_truncated", "db_truncated"}
            if (not isinstance(event, dict) or set(event) != expected_fields
                    or event["seq"] != seq or event["tool"] != "recall_context"
                    or event["db"] != db or event["db_id"] != db_ids[db]
                    or event["node_ids"] != expected_ids
                    or event["node_ids_truncated"] is not False or event["db_truncated"] is not False):
                raise RuntimeError(f"activity does not match packed {db} context IDs: {event!r}")
        tail = read_activity(8, activity["next_after"])
        if (tail.get("events") != [] or tail.get("next_after") != 2
                or tail.get("latest_seq") != 2 or tail.get("instance") != activity.get("instance")):
            raise RuntimeError(f"activity polling changed its own stream: {tail!r}")

        require_minimum(
            "project primary lane",
            project["primary"],
            args.expect_project_primary_min,
        )
        require_minimum(
            "user primary lane", user["primary"], args.expect_user_primary_min
        )
        require_exact(
            "project primary lane", project["primary"], args.expect_project_primary
        )
        require_exact("user primary lane", user["primary"], args.expect_user_primary)

        assert process.stdin is not None
        process.stdin.close()
        process.wait(timeout=args.timeout)
        assert process.stdout is not None
        extra_stdout = process.stdout.read()
        assert process.stderr is not None
        stderr = process.stderr.read()
        if process.returncode != 0:
            raise RuntimeError(f"MCP server exited with {process.returncode}: {stderr}")
        if extra_stdout:
            raise RuntimeError(f"MCP server emitted unexpected stdout: {extra_stdout!r}")

        expected_banner = (
            "mneme-mcp: capability profile "
            f"{args.capability_profile} (ambient authority only; not caller authentication)\n"
            "mneme-mcp: serving databases [project, user] over stdio\n"
        )
        if stderr != expected_banner:
            raise RuntimeError(f"unexpected MCP stderr: {stderr!r}")

        print(
            json.dumps(
                {
                    "schema_version": 1,
                    "protocol_version": PROTOCOL_VERSION,
                    "capability_profile": args.capability_profile,
                    "advertised_tools": sorted(advertised),
                    "databases": names,
                    "project": project,
                    "user": user,
                    "activity": {"schema": activity["schema"], "events": len(events),
                                 "next_after": tail["next_after"], "poll_emits": False},
                    "stderr": stderr.rstrip("\n"),
                    "exit_code": process.returncode,
                },
                indent=2,
                sort_keys=True,
            )
        )
        return 0
    except (KeyError, OSError, ValueError, RuntimeError, subprocess.TimeoutExpired) as error:
        stop_process(process)
        print(f"mcp_stdio_smoke: {error}", file=sys.stderr)
        return 1


if __name__ == "__main__":
    raise SystemExit(main())
