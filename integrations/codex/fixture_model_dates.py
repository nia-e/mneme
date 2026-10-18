"""Independent expected JSON for synthetic episodes inside the first epoch second."""
import copy


def epoch_second_display(card):
    """Fixtures use tiny milliseconds; all readable dates are the known epoch."""
    result = copy.deepcopy(card)
    occurred = result.pop("occurred")
    times = {key: result.pop(key) for key in ("recorded_at", "edition_recorded_at")}
    times.update({key: value for key, value in occurred.items() if key != "kind"})
    assert all(type(value) is int and 0 <= value < 1000 for value in times.values())
    result["occurred_utc"] = {"kind": occurred["kind"], **{
        key + "_utc": "1970-01-01T00:00:00Z" for key in occurred if key != "kind"}}
    result.update(recorded_at_utc="1970-01-01T00:00:00Z",
                  edition_recorded_at_utc="1970-01-01T00:00:00Z")
    return result
