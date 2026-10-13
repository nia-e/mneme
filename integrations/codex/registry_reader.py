"""One bounded read/selection round across explicitly admitted memory owners.

Native IDs remain paired with their canonical owner throughout. Selector aliases
are ephemeral presentation keys: never endpoints, receipt keys or write targets.
"""
from pathlib import Path
import time

from librarian_policy import resolve
from reader_contract import plan, prepare, validate_answer
from target_policy import TargetPolicy


def _snapshot(config):
    from registry_runtime import selection
    return selection(config)


def _current(config, snapshot, *, operation=None, store_name=None):
    current = _snapshot(config)
    if current.digest != snapshot.digest:
        raise ValueError("selected owner policy changed")
    if operation is not None:
        subsets = {"read": current.readable, "core": current.core,
                   "record": current.recordable, "maintenance": current.maintainable,
                   "recall": getattr(current, "recallable", current.readable)}
        if operation not in subsets or not any(store.name == store_name for store in subsets[operation]):
            raise ValueError("selected owner does not permit this operation")
    return current


def _alias(card):
    return card["db_id"] + ":" + card["id"]


def _key(card):
    # Episode IDs here are exact edition IDs, not their current editorial heads.
    return card["db_id"], card["id"]


def _observation(native, owner):
    from routing_memory import validate_observed_binding, validate_conditional_binding
    paths, routing, conditional = {}, {}, {}
    observation = native.get("observation")
    if not (isinstance(observation, dict) and observation.get("schema") == 1
            and observation.get("learning") == "disabled"
            and isinstance(observation.get("cards"), list)):
        return paths, routing, conditional
    for row in observation["cards"]:
        if not isinstance(row, dict) or not isinstance(row.get("node_id"), str):
            continue
        key = owner, row["node_id"]
        if row.get("entry_kind") == "conditional":
            conditional[key] = None
            if row.get("graph_path") in (None, []) and row.get("routing_binding") is None:
                try:
                    conditional[key] = validate_conditional_binding(
                        row.get("conditional_binding"), row["node_id"], expected_db_id=owner)
                except (ValueError, TypeError, KeyError, UnicodeError):
                    pass
            continue
        if "entry_kind" in row or "conditional_binding" in row:
            continue
        path = row.get("graph_path")
        from hook_recall import _bytes
        if isinstance(path, list) and len(path) <= 4 and _bytes(path) <= 1200:
            paths[key] = path
            if path and row.get("routing_binding") is not None:
                try:
                    binding = validate_observed_binding(row["routing_binding"], path, expected_db_id=owner)
                    if binding["route"]["target"] == row["node_id"] and binding["db_id"] == owner:
                        routing[key] = binding
                except (ValueError, TypeError, KeyError, UnicodeError):
                    pass
    return paths, routing, conditional


def execute(config, cue, runtime, reserve, emitted, *, start_owner=None):
    """Collect within one native ledger, then make at most one selector call."""
    from hook_recall import collect_reader, _bytes, _unknown_discovery, MAX_READER_BYTES, NODE_ID_RE
    from reader_worker import _was_emitted
    from hooks import _valid_card
    from turn_observer import validate_concern_row

    budget = resolve(config)
    diagnostic = {"pool_stage": "postpack_unseen", "pool": None, "stage": "native",
                  "selected_ids": [], "discovery": _unknown_discovery("multiple_owners"),
                  "routing_mode": "native_only", "concern_mode": "warning_only"}
    coverage = {"selected": 0, "attempted": 0, "unavailable": 0,
                "budget_omitted": 0, "complete": False}
    def done(value):
        return {**value, "selection": diagnostic, "read_coverage": coverage,
                "routing_mode": "native_only"}
    failed = {"outcome": "unavailable", "cards": [], "provider_attempt": False}
    try:
        selection = _snapshot(config)
    except (ValueError, OSError, TypeError, KeyError):
        return done(failed)
    read_plan, reason = plan([{"role": "user", "text": cue}], budget=budget)
    if read_plan is None or not read_plan["max_nodes"]:
        return done({**failed, "outcome": "skipped" if read_plan is None else "empty"})

    shared = {"deadline": time.monotonic() + budget.native_seconds, "decoded_bytes": 0}
    cards, owners, paths, routing, conditional, concern_rows, case_keys = [], set(), {}, {}, {}, [], set()
    owner_diagnostics = []
    candidate_keys, seen_count, content_full = set(), 0, False
    readable = list(getattr(selection, "recallable", selection.readable))
    if start_owner is not None:
        for index, store in enumerate(readable):
            if store.name == start_owner:
                readable = readable[index:] + readable[:index]
                break
    coverage["selected"] = len(readable)
    for store in readable:
        remaining = shared["deadline"] - time.monotonic()
        if (content_full or remaining <= 0 or shared.get("accounting_unknown")
                or shared["decoded_bytes"] >= budget.native_read_bytes):
            coverage["budget_omitted"] += 1
            owner_diagnostics.append({"name": store.name, "db_id": store.db_id, "outcome": "budget"})
            continue
        coverage["attempted"] += 1
        try:
            # Complete selection and operation admission precede every contact.
            _current(config, selection, operation="recall" if store.native_binding is not None else "read", store_name=store.name)
            target = TargetPolicy.from_selected(store, selection)
            native = collect_reader(Path(store.service_config), store.read.cue or cue,
                Path(selection.cwd), timeout=min(remaining, budget.native_seconds), budget=budget,
                read_plan=read_plan, store_target=target, shared_work=shared)
        except (ValueError, OSError, TypeError, KeyError):
            native = {"outcome": "unavailable", "cards": []}
        outcome = native.get("outcome", "unavailable")
        owner = native.get("db_id")
        if (outcome in ("ok", "empty") and (not isinstance(owner, str)
                or not NODE_ID_RE.fullmatch(owner) or store.db_id is not None and owner != store.db_id)):
            outcome = "unavailable"
        owner_diagnostics.append({"name": store.name, "db_id": store.db_id or owner,
                                  "outcome": outcome})
        if outcome not in ("ok", "empty"):
            coverage["unavailable"] += 1
            continue
        if owner in owners:
            owner_diagnostics[-1]["outcome"] = "duplicate_owner"
            coverage["unavailable"] += 1
            continue
        owners.add(owner)
        observed = _observation(native, owner)
        paths.update(observed[0]); routing.update(observed[1]); conditional.update(observed[2])
        for raw in native.get("concern_rows", []):
            try:
                row = validate_concern_row(raw)
                concern_rows.append({"db_id": owner, "row": row})
                case_keys.update((owner, endpoint["id"]) for endpoint in row["notice"]["binding"]["endpoints"])
            except (ValueError, TypeError, KeyError):
                pass
        for raw in native.get("cards", []):
            if not isinstance(raw, dict):
                diagnostic["invalid_count"] = diagnostic.get("invalid_count", 0) + 1
                continue
            card = {**raw, "db_id": owner, "scope": "named", "store_name": store.name,
                    "store_purpose": store.purpose}
            if not _valid_card(card):
                diagnostic["invalid_count"] = diagnostic.get("invalid_count", 0) + 1
                continue
            key = _key(card)
            if key in candidate_keys:
                continue
            candidate_keys.add(key)
            # Filter after this owner's verified concern endpoints are known, but
            # before packing: old deliveries must not crowd out fresh owners.
            if _was_emitted(card, emitted) and key not in case_keys:
                seen_count += 1
                continue
            # Identities and purpose share one retained content envelope. Native
            # input bytes are independently shared above. Stop on the first real
            # overflow rather than searching later owners/cards for smaller fits.
            if content_full or _bytes({"cards": cards + [card]}) > MAX_READER_BYTES - 16:
                content_full = True
                diagnostic["content_omitted_count"] = diagnostic.get("content_omitted_count", 0) + 1
                continue
            cards.append(card)
    coverage["complete"] = coverage["unavailable"] == 0 and coverage["budget_omitted"] == 0
    diagnostic["owners"] = owner_diagnostics
    unknown = shared.get("accounting_unknown", False)
    diagnostic["native_work"] = {"decoded_bytes": None if unknown else shared["decoded_bytes"],
        "read_allowance_bytes": budget.native_read_bytes,
        "read_allowance_exhausted": unknown or shared["decoded_bytes"] >= budget.native_read_bytes,
        **({"known_decoded_bytes": shared["decoded_bytes"], "accounting_unknown": True} if unknown else {})}
    diagnostic["postpack_count"] = len(cards)
    diagnostic["requested_effort"] = budget.effort
    diagnostic["stage"] = "filter"
    diagnostic["seen_count"] = seen_count
    diagnostic["pool"] = [{"id": c["id"], "fingerprint": c["fingerprint"], "db_id": c["db_id"]} for c in cards]
    if not cards:
        outcome = "empty" if coverage["complete"] else "unavailable"
        if diagnostic.get("content_omitted_count"):
            outcome = "omitted"
        elif not seen_count and diagnostic.get("invalid_count"):
            outcome = "unavailable"
        return done({**failed, "outcome": outcome})
    reader_cards = []
    for card in cards:
        key = _key(card)
        native = ({"entry_kind": "conditional"} if key in conditional else
                  {"graph_path": paths[key]} if key in paths else None)
        reader_cards.append({**card, "id": _alias(card), "native_id": card["id"],
                             **({"native": native} if native is not None else {})})
    prompt, offered = prepare(read_plan["dialogue"], reader_cards, concern_rows=concern_rows, budget=budget)
    if prompt is None:
        diagnostic["selector_reason"] = offered
        return done({**failed, "outcome": "omitted" if offered in ("prompt_cap", "empty_or_seen") else "unavailable"})
    diagnostic["prompt_omitted_count"] = offered["prompt_omitted_count"]
    diagnostic["path_omitted_count"] = len(offered["path_omitted_ids"])
    diagnostic["entry_omitted_count"] = len(offered["entry_omitted_ids"])
    reader_cards = [c for c in reader_cards if c["id"] in offered["ids"]]
    cards = [c for c in cards if _alias(c) in offered["ids"]]
    for card in reader_cards:
        key = card["db_id"], card["native_id"]
        if card["id"] in offered["path_omitted_ids"]:
            card.pop("native", None); routing.pop(key, None)
        if card["id"] in offered["entry_omitted_ids"]:
            card.pop("native", None); conditional.pop(key, None)
    diagnostic["pool"] = [{"id": c["id"], "fingerprint": c["fingerprint"], "db_id": c["db_id"]} for c in cards]
    try:
        _current(config, selection)
    except (ValueError, OSError, TypeError, KeyError):
        return done(failed)
    if not reserve():
        return done({**failed, "outcome": "budget"})
    diagnostic["stage"] = "selector"
    try:
        result = runtime.select(read_plan["dialogue"], reader_cards,
                                **({"concern_rows": concern_rows} if concern_rows else {}))
    except Exception:
        return done({**failed, "provider_attempt": True, "usage": None})
    if not isinstance(result, dict):
        return done({**failed, "provider_attempt": True, "usage": None})
    paid = {"usage": result.get("usage"), "provider_attempt": result.get("provider_attempt") is True,
            "elapsed_ms": result.get("elapsed_ms")}
    selected, nominations = [], []
    lookup = {_alias(c): c for c in cards}
    try:
        _current(config, selection)
        checked = validate_answer({"selected_ids": result.get("selected_ids", []),
                                   "concerns": result.get("concerns", [])},
                                  {"ids": list(lookup), "answer_bytes": budget.selector_answer_bytes})
        if result.get("reason") == "selected":
            for alias, card in lookup.items():
                if alias in checked["selected_ids"]:
                    key = _key(card)
                    selected.append({**card,
                        **({"routing_binding": routing[key]} if key in routing else {}),
                        **({"entry_kind": "conditional"} if key in conditional else {}),
                        **({"conditional_binding": conditional[key]} if conditional.get(key) is not None else {})})
            for concern in checked["concerns"]:
                left, right = lookup[concern["left_id"]], lookup[concern["right_id"]]
                # A native concern belongs to one owner. Cross-owner comparisons
                # never manufacture a curation case or authority in either store.
                if left["db_id"] == right["db_id"]:
                    nominations.append({**concern, "db_id": left["db_id"],
                                        "left_id": left["id"], "right_id": right["id"]})
    except (ValueError, OSError, TypeError, KeyError):
        return done({**failed, **paid})
    diagnostic["selected_ids"] = [c["id"] for c in selected]
    diagnostic["selected_keys"] = [[c["db_id"], c["id"]] for c in selected]
    # The registry's maintenance grant covers guarded tags only. Read-time
    # comparisons remain warning-only until a separate concern grant exists;
    # seeing a pair or a native notice is not authority to write a new notice.
    warnings = [{"shown_text": c["caveat"] + " Missing fact: " + c["missing_fact"],
                 "displayed_endpoint_ids": [c["left_id"], c["right_id"]],
                 "expected_row": None, "db_id": c["db_id"]} for c in nominations]
    return done({"outcome": "selected" if selected else "abstained" if result.get("reason") == "abstained" else "unavailable",
                 "cards": selected, "concerns": warnings, **paid})
