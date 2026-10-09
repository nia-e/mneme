"""Pure admission for owner-scoped tag stewardship; never contact an owner."""
import re

FIELDS = {"tag_stewardship", "tag_guide_id"}
CANONICAL_ID = re.compile(r"[0-7][0-9A-HJKMNP-TV-Z]{25}\Z")


def validate(value):
    enabled = value.get("tag_stewardship", False)
    if type(enabled) is not bool:
        raise ValueError("tag_stewardship must be a boolean")
    guide = value.get("tag_guide_id")
    if guide is not None and (not isinstance(guide, str) or CANONICAL_ID.fullmatch(guide) is None):
        raise ValueError("tag_guide_id must be a canonical same-owner node ID or null")
    return enabled, guide


def prepared_fields(recording_mode, enabled=None, guide=None):
    fields = {"tag_stewardship": recording_mode == "automatic" if enabled is None else enabled,
              "tag_guide_id": guide}
    validate({"recording_mode": recording_mode, **fields})
    return fields
