"""Shared prepared assessment policy, independent of the provider transport.

The recorder/router/steward contracts remain host-owned. A transport receives only
one bounded, prepared request and returns text plus accounting; it cannot weaken
validation or acquire native write authority. The caller reserves durable paid work.
"""
from dataclasses import dataclass
import math
import json
import time
import recording_contract
from rollout_primitives import decode_json

TURN_SECONDS = 45
MAX_FRAME_BYTES = 64 * 1024
MAX_RECORDING_FRAME_BYTES = 512 * 1024
MAX_ANSWER_BYTES = 4096
# Includes instructions, the largest authored source projection, and schema. This
# is a provider-independent encoded input fuse, not a token/cost guarantee.
MAX_PREPARED_BYTES = 128 * 1024
SCHEMA_PREFIX = "Return exactly one JSON value matching this schema, without markdown.\nOUTPUT_SCHEMA:\n"
PAYLOAD_PREFIX = "\nASSESSMENT_INPUT:\n"

@dataclass(frozen=True, slots=True)
class AssessmentRequest:
    instructions: str
    prompt: str
    output_schema: dict
    answer_byte_limit: int
    deadline: float

    def __post_init__(self):
        if (type(self.instructions) is not str or type(self.prompt) is not str
                or type(self.output_schema) is not dict or type(self.answer_byte_limit) is not int
                or not 0 < self.answer_byte_limit <= MAX_RECORDING_FRAME_BYTES
                or type(self.deadline) not in (int, float) or not math.isfinite(self.deadline)):
            raise TransportError("invalid_input")
        try:
            if len(self.instructions.encode()) + len(self.prompt_with_schema().encode()) > MAX_PREPARED_BYTES:
                raise TransportError("authored_input_limit")
        except (ValueError, TypeError, UnicodeError, RecursionError):
            raise TransportError("invalid_input") from None

    def prompt_with_schema(self):
        return (SCHEMA_PREFIX + json.dumps(self.output_schema, ensure_ascii=False, allow_nan=False,
                                          separators=(",", ":")) + PAYLOAD_PREFIX + self.prompt)

@dataclass(frozen=True, slots=True)
class AssessmentResponse:
    answer_text: str | None
    usage: dict | None
    model_identity: str | None
    outcome: str = "completed"

RUNTIME_DIAGNOSTIC_REASONS = frozenset({
    "unknown", "proposed", "abstained", "validation_failed", "invalid_output", "usage_unknown", "closed",
    "invalid_timeout", "invalid_config", "unavailable", "binary_changed", "transport_error",
    "model_mismatch", "unsupported_runtime", "frame_cap", "timeout", "invalid_events", "tool_activity", "provider_error", "thread_contract",
    "invalid_input", "invalid_observation", "source_turn_incomplete",
    "unsupported_source_host", "invalid_source_field", "duplicate_source_field", "unsupported_public_coverage",
    "unsupported_public_selection",
    "unsupported_public_omissions", "unsupported_json_depth", "unsupported_json_value",
    "invalid_source_reference", "source_item_limit", "source_bytes_limit", "missing_tool_result",
    "invalid_completion_boundary", "invalid_evidence_text", "evidence_item_limit",
    "unsupported_evidence_kind", "source_prompt_missing", "source_order_invalid", "invalid_message_shape",
    "unsupported_assistant_phase", "invalid_message_content", "invalid_content_slot", "invalid_tool_call",
    "duplicate_tool_call", "invalid_tool_result", "unmatched_tool_result", "tool_type_mismatch",
    "invalid_overlap_cards", "invalid_overlap_card", "static_prefix_limit", "authored_input_limit",
    "invalid_project_focus", "invalid_project_focus_scope",
})
_VALIDATOR_DIAGNOSTIC_REASONS = frozenset({
    "invalid_validation_context", "unsupported_json_depth", "unsupported_json_value", "answer_bytes_limit",
    "invalid_answer_shape", "invalid_proposal_shape", "invalid_proposal_text", "invalid_citation_count",
    "invalid_citation_shape", "invalid_citation_reference", "invalid_exact_quote", "duplicate_citation",
    "invalid_association_shape", "invalid_association_reference", "invalid_association_kind",
    "invalid_answer", "delivery_source_evidence_missing",
    "touchstone_nomination_disabled", "invalid_nomination_shape", "invalid_nomination_related",
    "destination_nomination_denied", "invalid_proposal_destination", "destination_text_limit",
    "destination_evidence_denied", "destination_root_user_evidence_required",
    "destination_association_denied", "association_owner_mismatch",
})
VALIDATION_DIAGNOSTIC_REASONS = _VALIDATOR_DIAGNOSTIC_REASONS | frozenset({
    "unknown", "answer_missing", "answer_type", "invalid_json",
})


def assessment_diagnostic(result):
    """Project only static host codes; never serialize a model value or exception.

    This optional diagnostic does not establish provenance or decide admission,
    retry, accounting, or a write. Its two ASCII codes fit within 256 JSON bytes.
    """
    unknown = {"runtime_reason": "unknown", "validation_reason": "unknown"}
    if type(result) is not dict:
        return unknown
    try:
        reason = result.get("reason")
        reason = (reason if type(reason) is str and len(reason) <= 64
                  and reason in RUNTIME_DIAGNOSTIC_REASONS else "unknown")
        validation = result.get("validation_reason")
        validation = (validation if reason == "invalid_output" and type(validation) is str and len(validation) <= 64
                      and validation in VALIDATION_DIAGNOSTIC_REASONS else "unknown")
        return {"runtime_reason": reason, "validation_reason": validation}
    except Exception:
        return unknown


class TransportError(Exception):
    def __init__(self, reason, *, validation_reason=None):
        super().__init__(reason)
        self.reason = reason
        self.validation_reason = (validation_reason if reason == "invalid_output"
                                  and type(validation_reason) is str
                                  and validation_reason in VALIDATION_DIAGNOSTIC_REASONS else "unknown")


def validate_response(response, request, context, validator):
    """One strict JSON and role validator for every assessment provider."""
    if response.outcome != "completed":
        raise TransportError(response.outcome)
    answer = response.answer_text
    if answer is None:
        raise TransportError("invalid_output", validation_reason="answer_missing")
    if type(answer) is not str:
        raise TransportError("invalid_output", validation_reason="answer_type")
    try:
        raw = answer.encode("utf-8")
    except UnicodeError:
        raise TransportError("invalid_output") from None
    if len(raw) > request.answer_byte_limit:
        raise TransportError("invalid_output", validation_reason="answer_bytes_limit")
    try:
        parsed = decode_json(raw)
    except (ValueError, TypeError, UnicodeError, RecursionError):
        raise TransportError("invalid_output", validation_reason="invalid_json") from None
    try:
        return validator(parsed, context)
    except (ValueError, TypeError, UnicodeError, RecursionError) as exc:
        reason = (exc.args[0] if type(exc) is ValueError and len(exc.args) == 1
                  and type(exc.args[0]) is str else None)
        reason = reason if reason in _VALIDATOR_DIAGNOSTIC_REASONS else "unknown"
        raise TransportError("invalid_output", validation_reason=reason) from None


def runtime(config, scratch_dir):
    """Select explicitly; never reinterpret an old Codex configuration."""
    assessor = config.get("assessor", "codex") if isinstance(config, dict) else getattr(config, "assessor", "codex")
    if assessor == "codex":
        from reader_runtime import ReaderRuntime
        return ReaderRuntime(config, scratch_dir)
    if assessor == "claude_code":
        from claude_runtime import ClaudeRuntime
        return ClaudeRuntime(config, scratch_dir)
    raise ValueError("unsupported assessor; reprepare configuration")


class AssessmentMixin:
    """Shared policy; subclasses own only process/thread lifecycle and transport."""
    def _assessment_seconds(self):
        return TURN_SECONDS

    def assess(self, observation, overlap_cards=None, *, timeout=TURN_SECONDS,
               recording_scope="project", expected_db_id=None, global_preferences_enabled=False,
               project_focus=None, tag_context=None, selected_stores=None) -> dict:
        """Assess one complete public projection, never perform a native write.

        A fresh recording-contract thread is used for every admitted assessment,
        even after another assessment. The process can survive success, but its
        thread cannot become a later reader/assessor conversation. A preflight
        refusal leaves an existing reader thread alone. The caller must reserve
        this one possible attempt durably before invoking us; no retry occurs.
        """
        receipt = self._fresh_assessment(
            lambda: recording_contract.prepare(observation, overlap_cards, budget=self.budget,
                recording_scope=recording_scope, expected_db_id=expected_db_id,
                global_preferences_enabled=global_preferences_enabled, project_focus=project_focus,
                **({"selected_stores": selected_stores} if selected_stores is not None else {}),
                **({"tag_context": tag_context} if tag_context is not None else {})),
            instructions=recording_contract.instructions_for, schema=recording_contract.schema_for,
            validator=recording_contract.validate_answer, field="assessment", service_name="mneme_recorder",
            success=lambda value: ("proposed" if value["proposal"] or value["maintenance"] or value.get("nomination") else
                                   "validation_failed" if (value.get("omissions", {}).get("proposal")
                                       or value.get("omissions", {}).get("maintenance")) else "abstained"), timeout=timeout,
            frame_bytes=MAX_RECORDING_FRAME_BYTES, answer_bytes=recording_contract.MAX_OUTPUT_BYTES)
        value = receipt.pop("assessment")
        if value is not None:
            value = dict(value)
            omissions = value.pop("omissions", None)
            if omissions is not None:
                receipt["intent_omissions"] = omissions
        receipt.update(value if value is not None else {"proposal": None, "maintenance": []})
        return receipt

    def steward(self, targets, tag_context, *, timeout=TURN_SECONDS) -> dict:
        """Classify one finite summary batch, on a fresh tool-free thread."""
        import stewardship_contract
        receipt = self._fresh_assessment(
            lambda: stewardship_contract.prepare(targets, tag_context, budget=self.budget),
            instructions=stewardship_contract.instructions, schema=stewardship_contract.schema,
            validator=stewardship_contract.validate_answer, field="stewardship",
            service_name="mneme_tag_steward", success=lambda _: "classified", timeout=timeout,
            frame_bytes=MAX_FRAME_BYTES,
            answer_bytes=(self.budget.stewardship_answer_bytes if self.budget else MAX_ANSWER_BYTES))
        value = receipt.pop("stewardship")
        receipt["decisions"] = value["decisions"] if value is not None else None
        return receipt

    def route(self, current, witnesses, *, expected_db_id, timeout=TURN_SECONDS) -> dict:
        """One fresh bounded matching call; never a write or selector continuation."""
        import routing_contract
        return self._fresh_assessment(
            lambda: routing_contract.prepare(current, witnesses, expected_db_id=expected_db_id, budget=self.budget),
            instructions=lambda _: routing_contract.INSTRUCTIONS, schema=routing_contract.schema,
            validator=routing_contract.validate_answer, field="routing", service_name="mneme_router",
            success=lambda value: "matched" if value.hints else "neutral", timeout=timeout,
            frame_bytes=MAX_FRAME_BYTES, answer_bytes=(self.budget.routing_answer_bytes if self.budget is not None
                          else routing_contract.MAX_OUTPUT_BYTES))

    def _fresh_assessment(self, preparation, *, instructions, schema, validator, field,
                          service_name, success, timeout, frame_bytes, answer_bytes):
        started = time.monotonic()
        receipt = {field: None, "reason": "provider_error", "usage": None,
                   "elapsed_ms": 0.0, "provider_attempt": False}
        with self._lock:
            preflight, clean_success, parked = True, False, None
            try:
                if self._closed or self._halted:
                    raise TransportError("closed" if self._closed else "usage_unknown")
                if self.budget is None:
                    raise TransportError("invalid_config")
                if (type(timeout) not in (int, float) or not 0 < timeout <= self._assessment_seconds()
                        or not math.isfinite(timeout)):
                    raise TransportError("invalid_timeout")
                prompt, context = preparation()
                if prompt is None:
                    raise TransportError(context)
                request = AssessmentRequest(instructions(context), prompt, schema(context),
                                            answer_bytes, started + timeout)
                if time.monotonic() >= request.deadline:
                    raise TransportError("timeout")
                preflight = False
                parked = self._begin_assessment(frame_bytes)
                response = self._run_assessment(request, service_name=service_name)
                receipt["provider_attempt"] = self._assessment_attempted()
                # Preserve accounting even when model identity/output is rejected.
                self._current_usage = response.usage
                if response.usage is None:
                    raise TransportError("usage_unknown")
                if response.model_identity != self.budget.model:
                    raise TransportError("model_mismatch")
                value = validate_response(response, request, context, validator)
                receipt.update({field: value, "reason": success(value), "usage": response.usage})
                clean_success = True
            except TransportError as exc:
                receipt["reason"] = exc.reason
                if exc.reason == "invalid_output":
                    receipt["validation_reason"] = exc.validation_reason
                if not preflight:
                    receipt["provider_attempt"] = self._assessment_attempted()
                    receipt["usage"] = self._current_usage
                    if receipt["provider_attempt"] and receipt["usage"] is None:
                        self._halted = True
                    self._stop_process()
            except (OSError, ValueError, TypeError, OverflowError, RecursionError):
                receipt["reason"] = "provider_error"
                if not preflight:
                    receipt["provider_attempt"] = self._assessment_attempted()
                    receipt["usage"] = self._current_usage
                    if receipt["provider_attempt"] and receipt["usage"] is None:
                        self._halted = True
                    self._stop_process()
            finally:
                if not preflight:
                    self._finish_assessment(parked, clean_success)
                receipt["elapsed_ms"] = (time.monotonic() - started) * 1000
        return receipt
