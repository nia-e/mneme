"""Complete, same-owner canonical tag guidance, not authority or learning evidence.

The caller owns activation, connection, identity resolution and the aggregate
native budget. This helper never opens a store, discovers a role, or writes.
SAVE revalidation is a host stale-intent check, not an atomic native guard.
Project hippocampus instructions remain the existing local file, not node content.
"""
from dataclasses import dataclass
import hashlib
import json
import re
import time
import unicodedata

ULID = re.compile(r"[0-7][0-9A-HJKMNP-TV-Z]{25}\Z")
SHA = re.compile(r"[0-9a-f]{64}\Z")
CODEC = "mneme.routing-content.v2"
MAX_TEXT_BYTES = 2048
ROLES = ("tag_guide",)
OUTCOMES = frozenset({"not_configured", "ok", "missing", "invalid", "incomplete",
                      "unavailable", "budget_omitted", "stale"})


def encoded(value):
    return json.dumps(value, sort_keys=True, separators=(",", ":"),
                      ensure_ascii=False, allow_nan=False).encode()


def _binding(value):
    # A selected snapshot can contain many owners. Freeze its exact digest,
    # not another full copy of every owner's config inside each role witness.
    return None if value is None else {"sha256": hashlib.sha256(encoded(value)).hexdigest()}


def selectors(value):
    """Static admission, without contacting even an active owner."""
    if value is None:
        return {}
    if not isinstance(value, dict) or set(value) - set(ROLES):
        raise ValueError("invalid_guidance_selectors")
    result = {}
    for role, selector in value.items():
        if (not isinstance(selector, dict) or set(selector) != {"id", "content"}
                or not isinstance(selector["id"], str) or not ULID.fullmatch(selector["id"])
                or selector["content"] != "summary"):
            raise ValueError("invalid_guidance_selectors")
        result[role] = dict(selector)
    return result


def _text(value):
    return (isinstance(value, str) and bool(value.strip())
            and len(value.encode()) <= MAX_TEXT_BYTES
            and not any(unicodedata.category(c) == "Cc" and c not in "\n\r\t" for c in value))


def _witness(node, selector, *, db_id, alias):
    if node is None or isinstance(node, dict) and node.get("found") is False:
        raise ValueError("missing")
    if isinstance(node, dict) and "_mneme_source" in node:
        # An exact retained edition can inform a labelled foreground read, but
        # cannot prove that live authored guidance still permits an intent.
        raise ValueError("unavailable")
    if (not isinstance(node, dict) or node.get("db") != alias or node.get("db_id") != db_id
            or node.get("id") != selector["id"] or node.get("memory_kind") != {"kind": "semantic"}
            or node.get("content_fingerprint_codec") != CODEC
            or not isinstance(node.get("content_fingerprint"), str)
            or not SHA.fullmatch(node["content_fingerprint"])):
        raise ValueError("invalid")
    field = "summary"
    text = node.get(field)
    if node.get("summary_truncated") is not False:
        raise ValueError("incomplete")
    if not _text(text):
        raise ValueError("invalid")
    result = {"id": selector["id"], "content": field, "text": text,
              "text_sha256": hashlib.sha256(text.encode()).hexdigest(),
              "content_fingerprint": node["content_fingerprint"],
              "content_fingerprint_codec": CODEC,
              "provenance": node.get("provenance"), "complete": True}
    # Freeze returned provenance exactly, never normalize it into authored claims.
    if len(encoded(result)) > 8 * 1024:
        raise ValueError("invalid")
    return result


@dataclass(frozen=True)
class GuidanceSnapshot:
    witness_json: str
    decoded_bytes: int = 0
    native_seconds_used: float = 0

    def to_dict(self):
        return json.loads(self.witness_json)

    snapshot = to_dict

    @property
    def roles(self):
        return self.to_dict()["roles"]

    @property
    def content_guards(self):
        guards = {}
        for role in self.roles.values():
            witness = role.get("witness")
            if role["outcome"] == "ok" and witness["content"] == "summary":
                guards[witness["id"]] = {"id": witness["id"],
                                        "content_fingerprint": witness["content_fingerprint"]}
        return list(guards.values())


def collect_guidance(owner, selected, *, owner_binding=None, roles=ROLES):
    """Read through an already authorized owner, charging its shared allowance."""
    selected = selectors(selected)
    roles = tuple(roles)
    if len(set(roles)) != len(roles) or set(roles) - set(ROLES):
        raise ValueError("invalid_guidance_roles")
    if not isinstance(owner.db_id, str) or not ULID.fullmatch(owner.db_id):
        raise ValueError("invalid_guidance_owner")
    alias = owner.policy.db_alias
    if not isinstance(alias, str) or not alias:
        raise ValueError("invalid_guidance_owner")
    before = getattr(owner, "remaining_bytes", None)
    start = time.monotonic()
    result, cache, measured = {}, {}, 0
    for role in roles:
        selector = selected.get(role)
        if selector is None:
            result[role] = {"outcome": "not_configured"}
            continue
        key = (selector["id"], selector["content"])
        if key not in cache:
            try:
                if (getattr(owner, "remaining_bytes", 1) <= 0
                        or time.monotonic() >= getattr(owner, "deadline", float("inf"))):
                    raise TimeoutError("native_budget")
                args = {"db": alias, "expected_db_id": owner.db_id, "id": selector["id"]}
                before_call = getattr(owner, "remaining_bytes", None)
                node = owner.call("get", args)
                measured += len(encoded(node))
                for r, s in selected.items():
                    if r in roles and s["id"] == selector["id"]:
                        try:
                            cache[(s["id"], s["content"])] = {"outcome": "ok", "witness": _witness(node, s, db_id=owner.db_id, alias=alias)}
                        except ValueError as error:
                            cache[(s["id"], s["content"])] = {"outcome": str(error)}
            except Exception as error:
                from mcp_client import McpToolError
                missing = isinstance(error, McpToolError) and str(error) == "node not found"
                outcome = ("missing" if missing else "budget_omitted" if isinstance(error, TimeoutError)
                           or "budget" in str(error) or "deadline" in str(error) else "unavailable")
                # Unknown read accounting exhausts the optional read allowance.
                if hasattr(owner, "remaining_bytes") and getattr(owner, "remaining_bytes") == locals().get("before_call", before):
                    owner.remaining_bytes = 0
                for r, s in selected.items():
                    if r in roles and s["id"] == selector["id"]:
                        cache[(s["id"], s["content"])] = {"outcome": outcome}
        result[role] = {"selector": dict(selector), **cache[key]}
    charged = max(0, before - owner.remaining_bytes) if before is not None else measured
    value = {"schema": "mneme.host-guidance.v1", "db_id": owner.db_id, "db": alias,
             "owner_binding": _binding(owner_binding), "roles": result}
    return GuidanceSnapshot(encoded(value).decode(), charged, max(0, time.monotonic() - start))


def revalidate_guidance(snapshot, owner, selected, *, owner_binding=None):
    """Refuse stale tag guidance. This is explicitly non-atomic for SAVE."""
    value = snapshot.to_dict() if isinstance(snapshot, GuidanceSnapshot) else snapshot
    if (not isinstance(value, dict) or value.get("schema") != "mneme.host-guidance.v1"
            or value.get("db_id") != owner.db_id or value.get("db") != owner.policy.db_alias
            or value.get("owner_binding") != _binding(owner_binding) or not isinstance(value.get("roles"), dict)):
        raise ValueError("guidance_stale")
    validate_snapshot(value)
    selected = selectors(selected)
    if any(row.get("selector") != selected.get(role) for role, row in value["roles"].items()):
        raise ValueError("guidance_stale")
    current = collect_guidance(owner, selected, owner_binding=owner_binding, roles=tuple(value["roles"]))
    if any(row["outcome"] not in ("ok", "not_configured") for row in current.roles.values()):
        raise ValueError("guidance_unavailable")
    if current.to_dict() != value:
        raise ValueError("guidance_stale")
    return current


def validate_snapshot(value):
    """Validate persisted/adapter witnesses before using prose or native guards."""
    if isinstance(value, GuidanceSnapshot):
        value = value.to_dict()
    if (not isinstance(value, dict) or set(value) != {"schema", "db_id", "db", "owner_binding", "roles"}
            or value["schema"] != "mneme.host-guidance.v1"
            or not isinstance(value["db_id"], str) or not ULID.fullmatch(value["db_id"])
            or not isinstance(value["db"], str) or not value["db"]
            or value["owner_binding"] is not None and (not isinstance(value["owner_binding"], dict)
                or set(value["owner_binding"]) != {"sha256"}
                or not isinstance(value["owner_binding"]["sha256"], str)
                or not SHA.fullmatch(value["owner_binding"]["sha256"]))
            or not isinstance(value["roles"], dict) or set(value["roles"]) - set(ROLES)
            or len(encoded(value)) > 24 * 1024):
        raise ValueError("invalid_guidance_snapshot")
    for role, row in value["roles"].items():
        if not isinstance(row, dict) or row.get("outcome") not in OUTCOMES:
            raise ValueError("invalid_guidance_snapshot")
        if row["outcome"] == "not_configured":
            if set(row) != {"outcome"}:
                raise ValueError("invalid_guidance_snapshot")
            continue
        selector = selectors({role: row.get("selector")})[role]
        if row["outcome"] != "ok":
            if set(row) != {"outcome", "selector"}:
                raise ValueError("invalid_guidance_snapshot")
            continue
        w = row.get("witness")
        if (set(row) != {"outcome", "selector", "witness"} or not isinstance(w, dict)
                or set(w) - {"id", "content", "text", "text_sha256", "content_fingerprint",
                             "content_fingerprint_codec", "provenance", "complete"}
                or w.get("id") != selector["id"] or w.get("content") != selector["content"]
                or w.get("complete") is not True or w.get("content_fingerprint_codec") != CODEC
                or not _text(w.get("text")) or "provenance" not in w
                or w.get("text_sha256") != hashlib.sha256(w["text"].encode()).hexdigest()
                or not isinstance(w.get("content_fingerprint"), str)
                or not SHA.fullmatch(w["content_fingerprint"])
):
            raise ValueError("invalid_guidance_snapshot")
    return value


class GuidanceOwner:
    """Finite reader on the caller's legitimate selected connection.

    Identity and operation eligibility are resolved by the caller before this
    adapter is created. Remaining allowance is passed in, never replenished.
    """
    def __init__(self, policy, service, db_id, deadline, remaining_bytes, *, operation="record"):
        from service import client_for_service
        import os
        self.policy, self.db_id = policy, db_id
        self.deadline, self.remaining_bytes = deadline, remaining_bytes
        token = os.environ.get(service.token_env) if service.token_env else None
        if service.token_env and not token:
            raise ValueError("owner_unavailable")
        timeout = max(.001, min(30, deadline - time.monotonic()))
        self.client = (service.native_client(timeout=timeout, operation=operation)
                       if getattr(policy, "native", False)
                       else client_for_service(service, token=token, timeout=timeout))

    def __enter__(self):
        self.policy.validate_workspace()
        self.client.connect()
        return self

    def __exit__(self, *_):
        self.client.timeout = max(.001, min(.1, self.deadline - time.monotonic()))
        self.client.close()

    def call(self, name, args):
        self.policy.validate_workspace()
        left = self.deadline - time.monotonic()
        if left <= 0 or self.remaining_bytes <= 0:
            raise TimeoutError("native_budget")
        self.client.timeout = min(30, left)
        if getattr(self.policy, "native", False):
            self.client.max_native_bytes = self.remaining_bytes
        before = getattr(self.client, "work_bytes", None)
        before_remaining = self.remaining_bytes
        try:
            value = self.client.call_tool(name, {**args, "db": self.policy.db_alias,
                                                "expected_db_id": self.db_id})
            charged = (self.client.work_bytes - before if type(before) is int else len(encoded(value)))
            if type(charged) is not int or charged < 0:
                raise ValueError("native_accounting_unknown")
            self.remaining_bytes -= charged
            if self.remaining_bytes < 0:
                raise ValueError("native_byte_budget")
            return value
        except Exception:
            current = getattr(self.client, "work_bytes", None)
            if type(before) is int and type(current) is int and current > before:
                self.remaining_bytes = before_remaining-(current-before)
                if self.remaining_bytes < 0:
                    raise ValueError("native_byte_budget")
            else:
                self.remaining_bytes = 0
            raise
