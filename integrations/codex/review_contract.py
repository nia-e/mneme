"""Pure, bounded suggestions for foreground memory judgment.

An effect is DATA, not a native request, approval token, or execution authority.
The enclosing source-bound nomination owns evidence, delivery, and retention.
This module owns only the closed effect union and exact offered-reference binding;
there is no queue, filesystem, provider, or owner connection here.
"""
from __future__ import annotations

from dataclasses import dataclass
import json
import re

_ULID = re.compile(r"[0-7][0-9A-HJKMNP-TV-Z]{25}\Z")
EFFECT_KINDS = (
    "touchstone", "core_creation", "core_membership", "core_content_change",
    "supersede", "forget", "reconcile", "merge", "episode_correction",
)


def _require(condition, reason):
    if not condition:
        raise ValueError(reason)


def _encoded(value):
    return json.dumps(value, ensure_ascii=False, sort_keys=True, separators=(",", ":"),
                      allow_nan=False).encode()


def _text(value, maximum, *, empty=False):
    _require(isinstance(value, str) and "\x00" not in value
             and (empty or bool(value.strip())) and len(value.encode()) <= maximum,
             "invalid_review_text")
    return value


def _limits(max_bytes, max_summary_bytes, max_body_bytes):
    _require(all(type(n) is int and n > 0 for n in
                 (max_bytes, max_summary_bytes, max_body_bytes))
             and max_summary_bytes <= max_bytes and max_body_bytes <= max_bytes,
             "invalid_review_limits")


@dataclass(frozen=True)
class ReviewReference:
    role: str
    opaque_id: str
    db_id: str
    id: str
    kind: str
    summary: str


@dataclass(frozen=True)
class ReviewEffect:
    kind: str
    references: tuple[ReviewReference, ...]
    details: tuple[tuple[str, str], ...]

    def to_model(self):
        """Reconstruct the proposed effect with the model's offered opaque IDs."""
        return {"kind": self.kind, **dict(self.details),
                **{ref.role: ref.opaque_id for ref in self.references}}

    def to_handoff(self):
        """Bound exact references, not an executable native operation payload."""
        return {"kind": self.kind, **dict(self.details), "targets": [
            {"role": ref.role, "db_id": ref.db_id, "id": ref.id,
             "kind": ref.kind, "summary": ref.summary} for ref in self.references]}


def effect_schema(*, max_summary_bytes, max_body_bytes):
    """Model-facing closed union; native foreground guards are never model input."""
    _limits(max(max_summary_bytes, max_body_bytes), max_summary_bytes, max_body_bytes)
    target = {"type": "string", "minLength": 1, "maxLength": 256}
    summary = {"type": "string", "minLength": 1, "maxLength": max_summary_bytes}
    body = {"type": "string", "maxLength": max_body_bytes}
    def branch(kind, properties):
        return {"type": "object", "additionalProperties": False,
                "required": ["kind", *properties],
                "properties": {"kind": {"const": kind}, **properties}}
    return {"oneOf": [
        branch("touchstone", {}),
        branch("core_creation", {"summary": summary, "body": body}),
        branch("core_membership", {"target": target,
            "membership": {"enum": ["promote", "demote"]}}),
        branch("core_content_change", {"target": target,
            "field": {"enum": ["summary", "body"]},
            "proposed_text": {"type": "string", "maxLength": max(max_summary_bytes, max_body_bytes)}}),
        branch("supersede", {"winner": target, "loser": target}),
        branch("forget", {"target": target}),
        branch("reconcile", {"a": target, "b": target,
            "resolution": {"enum": ["context-dependent", "unresolved"]}}),
        branch("merge", {"mode": {"const": "full"}, "winner": target, "loser": target}),
        branch("merge", {"mode": {"const": "keep"}, "a": target, "b": target}),
        branch("episode_correction", {"target": target, "summary": summary,
            "body": body, "reason": summary}),
    ]}


def validate_effect(value, offered_targets, *, destination_db_id, max_bytes,
                    max_summary_bytes, max_body_bytes):
    """Bind only offered targets in one destination; do not read or refresh them.

    Target summaries/IDs describe the nomination's observed context. Foreground
    execution must freshly inspect exact targets and use the operation's own
    guards. This does not promise an atomic approval snapshot for old ID-only APIs.
    """
    _limits(max_bytes, max_summary_bytes, max_body_bytes)
    _require(isinstance(destination_db_id, str) and _ULID.fullmatch(destination_db_id),
             "invalid_review_owner")
    _require(isinstance(value, dict) and len(_encoded(value)) <= max_bytes,
             "invalid_review_effect")
    kind = value.get("kind")
    _require(kind in EFFECT_KINDS, "unknown_review_effect")
    roles, fields = (), ()
    if kind == "core_creation":
        fields = ("summary", "body")
    elif kind == "core_membership":
        roles, fields = ("target",), ("membership",)
    elif kind == "core_content_change":
        roles, fields = ("target",), ("field", "proposed_text")
    elif kind in ("supersede", "merge"):
        if kind == "merge":
            _require(value.get("mode") in ("full", "keep"), "invalid_review_merge_mode")
            fields = ("mode",)
        roles = ("a", "b") if value.get("mode") == "keep" else ("winner", "loser")
    elif kind == "forget":
        roles = ("target",)
    elif kind == "reconcile":
        roles, fields = ("a", "b"), ("resolution",)
    elif kind == "episode_correction":
        roles, fields = ("target",), ("summary", "body", "reason")
    _require(set(value) == {"kind", *roles, *fields}, "invalid_review_effect_fields")
    if kind == "core_membership":
        _require(value["membership"] in ("promote", "demote"), "invalid_core_membership")
    if kind == "core_content_change":
        _require(value["field"] in ("summary", "body"), "invalid_core_content_field")
        _text(value["proposed_text"], max_summary_bytes if value["field"] == "summary"
              else max_body_bytes, empty=value["field"] == "body")
    if kind == "reconcile":
        _require(value["resolution"] in ("context-dependent", "unresolved"), "invalid_review_resolution")
    if kind in ("core_creation", "episode_correction"):
        _text(value["summary"], max_summary_bytes)
        _text(value["body"], max_body_bytes, empty=True)
    if kind == "episode_correction":
        _text(value["reason"], max_summary_bytes)

    # Match the recorder's existing AssociationTarget contract without importing
    # its enclosing proposal/observation module (which consumes this module).
    offered = {}
    for target in offered_targets:
        opaque = getattr(target, "opaque_id", None)
        _require(isinstance(opaque, str) and 0 < len(opaque.encode()) <= 256
                 and opaque not in offered, "invalid_review_offered_targets")
        offered[opaque] = target
    references = []
    identities = set()
    for role in roles:
        opaque = value[role]
        _require(isinstance(opaque, str) and opaque in offered, "review_target_not_offered")
        target = offered[opaque]
        identifier = getattr(target, "native_id", None)
        target_kind = getattr(target, "kind", None)
        _require(isinstance(identifier, str) and _ULID.fullmatch(identifier)
                 and getattr(target, "db_id", None) == destination_db_id
                 and getattr(target, "origin", None) in ("overlap", "delivery"),
                 "review_target_owner_or_identity")
        _require(target_kind == ("episode" if kind == "episode_correction" else "semantic"),
                 "review_target_kind")
        _require(identifier not in identities, "review_targets_must_be_distinct")
        identities.add(identifier)
        _text(target.summary, max_bytes)
        if getattr(target, "touchstone_json", None) is not None:
            _require(kind not in ("core_membership", "core_content_change")
                     and not (kind == "merge" and value["mode"] == "full"),
                     "review_cannot_rewrite_touchstone")
        references.append(ReviewReference(role, opaque, destination_db_id, identifier,
                                          target_kind, target.summary))
    result = ReviewEffect(kind, tuple(references), tuple((key, value[key]) for key in fields))
    _require(len(_encoded(result.to_handoff())) <= max_bytes, "review_handoff_bytes")
    return result


def check_effect(effect, offered_targets, **limits):
    """Revalidate an injected typed result against the same frozen offered targets."""
    _require(isinstance(effect, ReviewEffect), "review_effect_required")
    checked = validate_effect(effect.to_model(), offered_targets, **limits)
    _require(checked == effect, "review_effect_binding_changed")
    return checked


@dataclass(frozen=True)
class _FrozenTarget:
    opaque_id: str
    native_id: str
    db_id: str
    kind: str
    summary: str
    origin: str = "overlap"
    touchstone_json: str | None = None


def validate_handoff_effect(value, related, **limits):
    """Check persisted bound data without treating it as native approval.

    Every effect reference must match an independently frozen related item in
    the envelope. Native immutability/guards remain the foreground operation's
    responsibility; these summaries are historical, not a current preflight.
    """
    _require(isinstance(value, dict) and isinstance(value.get("targets"), list)
             and isinstance(related, list), "invalid_review_handoff")
    available = {(item["db_id"], item["id"]): item for item in related}
    model = {key: item for key, item in value.items() if key != "targets"}
    offered, roles = [], set()
    for index, reference in enumerate(value["targets"]):
        _require(isinstance(reference, dict)
                 and set(reference) == {"role", "db_id", "id", "kind", "summary"}
                 and isinstance(reference["role"], str) and reference["role"] not in roles,
                 "invalid_review_handoff_target")
        role = reference["role"]
        roles.add(role)
        _require(role not in model, "invalid_review_handoff_target")
        observed = available.get((reference["db_id"], reference["id"]))
        _require(observed is not None and all(reference[key] == observed[key]
                 for key in ("db_id", "id", "kind", "summary")), "review_handoff_target_changed")
        opaque = "frozen" + str(index)
        model[role] = opaque
        offered.append(_FrozenTarget(opaque, reference["id"], reference["db_id"],
                                     reference["kind"], reference["summary"]))
    checked = validate_effect(model, offered, **limits)
    _require(checked.to_handoff() == value, "review_handoff_changed")
    return checked
