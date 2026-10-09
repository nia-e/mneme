"""One immutable, explicit host policy for the async librarian's three roles.

These uncalibrated admission/work presets are not relevance quotas or guaranteed
provider/backend spending caps. Actual usage remains in the parent session ledger.
"""
from dataclasses import dataclass
from collections.abc import Mapping

MODEL = "gpt-6.1-sol"
EFFORTS = ("low", "medium", "high")
# selector prompt/answer, background latency seconds/read bytes, shared attempts,
# input/output tokens, ordinary-selector room before optional matcher work.
_PRESETS = (
    (8192, 2048, 10.0, 32768, 24, 125000, 10000, 8192, 2048),
    (12288, 4096, 15.0, 65536, 48, 250000, 20000, 16384, 4096),
    (24576, 8192, 20.0, 131072, 96, 500000, 40000, 32768, 8192),
)

@dataclass(frozen=True, slots=True)
class LibrarianBudget:
    # Defaults are an explicit convenience for pure, offline contract fixtures.
    # Configured jobs MUST use resolve(); it never supplies these defaults.
    model: str = MODEL
    effort: str = "medium"

    def __post_init__(self):
        if type(self.model) is not str or self.model != MODEL:
            raise ValueError("reader_model must explicitly select gpt-6.1-sol; reprepare async configuration")
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
    else:
        model = getattr(config, "reader_model", None)
        effort = getattr(config, "librarian_effort", None)
    return LibrarianBudget(model, effort)
