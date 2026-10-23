"""One immutable, explicit host policy for the async librarian's three roles.

These uncalibrated per-packet admission/work presets are not relevance quotas or
guaranteed provider/backend spending caps. Lifetime usage remains aggregate
telemetry; paid idle maintenance also retains its separate owner/day allowance.
"""
from dataclasses import dataclass
from collections.abc import Mapping
import re

MODEL = "gpt-6.1-sol"
EFFORTS = ("low", "medium", "high")
# Shared worker lifetime and maximum idle-maintenance refresh interval.
IDLE_SECONDS = 300
# Host-side decoded work, shared across selected owners in one native round.
# This is a ceiling, not a target or model context allowance. Native transport
# and bridge decoding both count; effort still governs prompts, time and tokens.
NATIVE_READ_BYTES = 512 * 1024
# selector prompt/answer, background latency seconds/read bytes, packet attempts,
# packet input/output tokens, ordinary-selector room before optional matcher work.
# Per-call bounds (including Claude's output-token setting) remain unchanged.
_PRESETS = (
    (8192, 2048, 10.0, NATIVE_READ_BYTES, 24, 125000, 10000, 8192, 2048),
    (12288, 4096, 15.0, NATIVE_READ_BYTES, 48, 250000, 20000, 16384, 4096),
    (24576, 8192, 20.0, NATIVE_READ_BYTES, 96, 500000, 40000, 32768, 8192),
)

@dataclass(frozen=True, slots=True)
class LibrarianBudget:
    # Defaults are an explicit convenience for pure, offline contract fixtures.
    # Configured jobs MUST use resolve(); it never supplies these defaults.
    model: str = MODEL
    effort: str = "medium"
    assessor: str = "codex"

    def __post_init__(self):
        if type(self.assessor) is not str or self.assessor not in ("codex", "claude_code"):
            raise ValueError("assessor must explicitly select codex or claude_code")
        if self.assessor == "codex":
            if type(self.model) is not str or self.model != MODEL:
                raise ValueError("reader_model must explicitly select gpt-6.1-sol; reprepare async configuration")
        elif (type(self.model) is not str or len(self.model) > 128
              or re.fullmatch(r"claude-(?:[a-z][a-z0-9]*-)+[0-9][a-z0-9-]*", self.model) is None):
            raise ValueError("reader_model must explicitly select a full Claude model ID, not an alias")
        if type(self.effort) is not str or self.effort not in EFFORTS:
            raise ValueError("librarian_effort must explicitly select low, medium, or high")

    def _value(self, index):
        return _PRESETS[EFFORTS.index(self.effort)][index]

    selector_prompt_bytes = property(lambda self: self._value(0))
    selector_answer_bytes = property(lambda self: self._value(1))
    # Optional recorder hints reuse prompt work room; source evidence keeps its
    # independent authored ceiling. This is a byte allowance, not a note quota.
    recording_hint_bytes = property(lambda self: self.selector_prompt_bytes)
    routing_prompt_bytes = property(lambda self: min(self.selector_prompt_bytes, 12 * 1024))
    routing_answer_bytes = property(lambda self: min(self.selector_answer_bytes, 4096))
    stewardship_prompt_bytes = property(lambda self: self.selector_prompt_bytes)
    stewardship_answer_bytes = property(lambda self: self.selector_answer_bytes)
    native_seconds = property(lambda self: self._value(2))
    native_read_bytes = property(lambda self: self._value(3))
    attempts = property(lambda self: self._value(4))
    input_tokens = property(lambda self: self._value(5))
    output_tokens = property(lambda self: self._value(6))
    selector_room_input_tokens = property(lambda self: self._value(7))
    selector_room_output_tokens = property(lambda self: self._value(8))

    def overlap_window(self, authored_room):
        """Optimistic summary-only capacity under current native API ceilings.

        Charge a minimum projection plus its array separator. Real
        cards, readback envelopes and optional history cost more; preparation
        and collection must charge their actual serialized bytes.
        """
        minimum_projection_bytes = len(b'{"id":"overlap001","kind":"episode","summary":"x"}') + 1
        capacity = min(256, max(0, min(authored_room, self.recording_hint_bytes,
                                     self.native_read_bytes)) // minimum_projection_bytes)
        return {"k": min(64, capacity), "max_nodes": capacity, "depth": 0}


def resolve(config):
    """Constructor-checked policy; missing, legacy or coerced fields fail closed."""
    if isinstance(config, Mapping):
        model, effort = config.get("reader_model"), config.get("librarian_effort")
        assessor = config.get("assessor", "codex")
    else:
        model = getattr(config, "reader_model", None)
        effort = getattr(config, "librarian_effort", None)
        assessor = getattr(config, "assessor", "codex")
    return LibrarianBudget(model, effort, assessor)
