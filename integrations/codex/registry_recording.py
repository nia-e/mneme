"""Selected-owner recording mechanics, sharing one source-turn resource envelope.

This is the registry adapter, not another recorder or inference loop. The durable
source admission, reservation, uncertain-intent handling and write lifecycle stay
in recording_jobs. Discovery never creates an owner or substitutes a destination.
"""
from dataclasses import dataclass, replace
from pathlib import Path
import time

from rollout_primitives import digest, encoded, read_regular
from store_registry import Selection, revalidate


class ReadBudgetRefused(ValueError):
    """A pre-SAVE read allowance refusal, never an ambiguous native mutation."""


def selection(config):
    from registry_runtime import selection as current_selection
    return current_selection(config)


def config_digest(config):
    """Bind the entire admitted selection, including purposes and service bytes."""
    selected = selection(config)
    hook_sha = None
    if config.get("_config_path") is not None:
        raw = read_regular(Path(config["_config_path"]), 64 * 1024)
        if len(raw) > 64 * 1024:
            raise ValueError("hook_config_cap")
        hook_sha = digest(raw)
    return digest(encoded({"selection": selected.to_dict(), "hook_sha256": hook_sha,
                           "recording_mode": config.get("recording_mode"),
                           "project_root": str(config["project_root"])}))


@dataclass
class Acquisition:
    selection: Selection
    stores: tuple
    cards: list
    decoded_bytes: int
    native_seconds_left: float
    diagnostic: dict

    @property
    def scope_options(self):
        return {"selected_stores": self.stores}

    def named(self, name):
        for store in self.stores:
            if store.name == name:
                return store
        raise ValueError("destination_not_offered")


def _charged(result, previous):
    work = result.get("native_work") if isinstance(result, dict) else None
    current = work.get("decoded_bytes") if isinstance(work, dict) else None
    if type(current) is not int or current < previous:
        raise ValueError("native_accounting_unknown")
    return current


def collect(config, observation, *, budget, seconds):
    """Identity-first acquisition; each owner consumes the same finite allowance.

    Verify every reachable recording destination before optional overlap work, so
    the first owner's hint pool cannot consume all opportunity to choose another
    destination. A missing owner is an omission, never another empty store.
    """
    from hook_recall import collect_overlap, resolve_project_identity
    from recording_contract import overlap_plan
    from target_policy import TargetPolicy

    selected = selection(config)
    deadline = time.monotonic() + min(budget.native_seconds, seconds)
    diagnostic = {"selected": len(selected.recordable), "offered": 0,
                  "identity_unavailable": 0, "budget_omitted": 0,
                  "overlap_omitted": 0, "native_accounting": "measured",
                  "routing": "not_enabled", "concerns": "not_enabled"}
    charged = 0
    available, cards = [], []
    exhausted = False
    for store in selected.recordable:
        remaining = deadline - time.monotonic()
        if exhausted or remaining <= 0 or charged >= budget.native_read_bytes:
            diagnostic["budget_omitted"] += 1
            continue
        policy = TargetPolicy.from_selected(store, selected)
        result = resolve_project_identity(Path(store.service_config), Path(config["project_root"]),
                    timeout=min(2, remaining), store_target=policy, budget=budget,
                    spent_native_bytes=charged)
        try:
            charged = _charged(result, charged)
        except ValueError:
            # A failed read with unknown cost cannot receive a new allowance.
            charged = budget.native_read_bytes
            exhausted = True
            diagnostic["native_accounting"] = "unknown"
        identifier = result.get("db_id")
        from store_registry import ULID
        if (exhausted or charged > budget.native_read_bytes or result.get("outcome") != "ok"
                or not isinstance(identifier, str) or not ULID.fullmatch(identifier)
                or store.db_id is not None and identifier != store.db_id):
            diagnostic["identity_unavailable"] += 1
            continue
        # Injected project adapters can be unpinned until their first live read.
        # Only this assessment uses the resolved ID; the original snapshot still
        # fences config/service admission and the job pins this native identity.
        recording = store.record
        if not store.read.enabled:
            recording = replace(recording, allow_associations=False)
        available.append(replace(store, db_id=identifier, record=recording))

    diagnostic["offered"] = len(available)
    plan, reason = overlap_plan(observation, budget=budget, selected_stores=tuple(available))
    if plan is None:
        raise ValueError("assessment_input_refused")
    cue = observation["evidence"][0]["content"][0]["text"]
    for store in available:
        if not store.read.enabled or not store.record.allow_associations:
            continue
        remaining = deadline - time.monotonic()
        if exhausted or remaining <= 0 or charged >= budget.native_read_bytes:
            diagnostic["overlap_omitted"] += 1
            continue
        policy = TargetPolicy.from_selected(selected.named(store.name), selected)
        result = collect_overlap(Path(store.service_config), store.read.cue or cue, Path(config["project_root"]),
                    expected_db_id=store.db_id, timeout=remaining, budget=budget,
                    read_plan=plan, spent_native_bytes=charged, store_target=policy)
        try:
            charged = _charged(result, charged)
        except ValueError:
            charged = budget.native_read_bytes
            exhausted = True
            diagnostic["native_accounting"] = "unknown"
        if (exhausted or charged > budget.native_read_bytes
                or result.get("outcome") not in ("ok", "empty") or result.get("db_id") != store.db_id):
            diagnostic["overlap_omitted"] += 1
            continue
        # Native IDs are unique only within an owner. The contract produces its
        # opaque IDs from these owner-qualified cards, never ID-only deduping.
        cards.extend({**card, "db_id": store.db_id} for card in result["cards"])
    revalidate(selected)
    return Acquisition(selected, tuple(available), cards, charged,
                       max(0, deadline - time.monotonic()), diagnostic)


def freeze_destination(acquisition, destination, *, seconds):
    from target_policy import TargetPolicy
    store = acquisition.named(destination)
    revalidate(acquisition.selection, operation="record", store_name=destination)
    policy = TargetPolicy.from_selected(acquisition.selection.named(destination), acquisition.selection)
    return store, policy, {"destination": destination, "db_id": store.db_id,
                          "db_alias": store.db_alias, "store_target": policy.canonical(),
                          "registry_native_bytes": acquisition.decoded_bytes,
                          "native_deadline_monotonic": time.monotonic() + min(
                              acquisition.native_seconds_left, max(0, seconds))}


def check_proposal(proposal, context):
    """Check even a runtime adapter's typed result against this exact prompt."""
    from recording_contract import Proposal, validate_answer
    if not isinstance(proposal, Proposal) or context.selected_stores is None:
        raise ValueError("registry_proposal_required")
    offered = next((store for store in context.selected_stores if store.name == proposal.destination), None)
    if offered is None:
        raise ValueError("destination_not_offered")
    value = {"kind": proposal.kind, "summary": proposal.summary, "body": proposal.body,
             "destination": proposal.destination, "evidence_ids": [item.evidence_id for item in proposal.evidence],
             "tags": [tag for tag in proposal.tags if tag not in offered.required_tags]}
    if proposal.kind == "lesson":
        value["associate_with"] = proposal.associate_with.opaque_id if proposal.associate_with else None
    checked = validate_answer({"proposal": value}, context)["proposal"]
    if checked != proposal:
        raise ValueError("registry_proposal_binding_changed")
    return checked


def validate_payload(config, job):
    """Recheck the stored intent before native SAVE, not just model JSON shape."""
    selected = selection(config)
    store = selected.named(job["destination"])
    revalidate(selected, operation="record", store_name=store.name)
    rule = store.record
    payload = job["payload"]
    if (encoded(job.get("frozen_selection")) != encoded(config["frozen_selection"])
            or store.db_id is not None and job["db_id"] != store.db_id
            or job.get("proposal_kind") not in rule.kinds
            or job.get("maintenance") or job.get("routing")
            or not isinstance(payload.get("summary"), str)
            or not 0 < len(payload["summary"].encode()) <= rule.max_summary_bytes
            or not isinstance(payload.get("body"), str)
            or not 0 < len(payload["body"].encode()) <= rule.max_body_bytes
            or not job.get("citations")
            or any(citation.get("kind") not in rule.evidence_kinds for citation in job["citations"])):
        raise ValueError("registry_recording_policy_changed")
    tags = payload.get("tags", [])
    if (not isinstance(tags, list) or len(tags) > 32 or len(set(tags)) != len(tags)
            or not set(rule.required_tags) <= set(tags)
            or "possibility" in tags and job["proposal_kind"] != "possibility"):
        raise ValueError("registry_recording_tags")
    from tag_context import ordinary_tags
    ordinary_tags([tag for tag in tags if tag not in rule.required_tags and tag != "possibility"], max_count=32)
    links = payload.get("links", [])
    if links:
        association = job.get("association", {})
        if (not rule.allow_associations or not store.read.enabled or job["proposal_kind"] != "lesson"
                or association.get("db_id") != job["db_id"]
                or association.get("outcome") != "requested"
                or links != [{"to": association.get("target"), "kind": "associative", "weight": 0.5}]):
            raise ValueError("registry_association_policy")
    return store


def receipt_target(job):
    """Retain historical owner evidence without reconstructing current config."""
    return {"scope": "configured", "name": job["destination"], "db_alias": job["db_alias"],
            "db_id": job["db_id"], "selection_digest": job["frozen_selection"]["digest"]}
