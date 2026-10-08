"""Offline admission/action-order qualification; no hooks, stores or providers."""
from __future__ import annotations

from copy import deepcopy
import contextlib
import io
import json
from pathlib import Path
import sys
import tempfile
import unittest

sys.path.insert(0, str(Path(__file__).resolve().parent))
import contextual_delivery_evidence as evidence

SESSION = "session-1"
TURN = "turn-1"
TEXT = 'Stored references:\n[{"id":"card-1","summary":"café \\"quoted\\""}]\r\n'


def packet(text=TEXT):
    return {"schema": evidence.PACKET_SCHEMA, "session_id": SESSION, "turn_id": TURN,
            "rendered_text": text, "rendered_sha256": evidence.digest(text.encode()),
            "caller_binding": {"request_sha256": "b" * 64, "epoch": 3, "fence": "fence-1"}}


def row(kind, payload):
    return {"type": kind, "payload": payload}


def event(kind, turn=TURN):
    return row("event_msg", {"type": kind, "turn_id": turn})


def admission(text=TEXT, *, turn=TURN):
    return row("response_item", {"type": "message", "id": "admission-1", "role": "developer",
        "content": [{"type": "input_text", "text": text}],
        evidence.META: {"turn_id": turn, "content_item_kinds": ["hooks.additional_context"]}})


def call(identifier="later", *, typ="custom_tool_call", turn=TURN, text="inspect()"):
    return row("response_item", {"type": typ, "id": "call-item-" + identifier,
        "call_id": identifier, "name": "exec", evidence.CALL_FIELDS[typ]: text,
        evidence.META: {"turn_id": turn}})


def output(identifier="later", *, typ="custom_tool_call_output", turn=TURN, text="observed result"):
    return row("response_item", {"type": typ, "id": "result-item-" + identifier,
        "call_id": identifier, "output": text, evidence.META: {"turn_id": turn}})


def public_message(identifier="answer", *, phase="final_answer", turn=TURN, text="Public answer"):
    return row("response_item", {"type": "message", "id": identifier, "role": "assistant",
        "phase": phase, "content": [{"type": "output_text", "text": text}],
        evidence.META: {"turn_id": turn}})


def task_prompt(text="Update the project for R5.", *, identifier="task-prompt", turn=TURN):
    return row("response_item", {"type": "message", "id": identifier, "role": "user",
        "content": [{"type": "input_text", "text": text}],
        evidence.META: {"turn_id": turn, "content_item_kinds": ["user.text"]}})


def prefix():
    return [row("session_meta", {"id": SESSION}), event("task_started"),
            row("turn_context", {"turn_id": TURN})]


def sequence():
    return prefix() + [call("trigger"), admission(), output("trigger"),
                       call(), output(), event("task_complete")]


def encoded(rows):
    return b"".join(json.dumps(r, ensure_ascii=False).encode() + b"\n" for r in rows)


class DeliveryEvidenceTests(unittest.TestCase):
    def qualify(self, rows=None, emission=None, **kwargs):
        return evidence.qualify(emission or packet(), encoded(rows or sequence()), **kwargs)

    def unknown(self, result, reason):
        self.assertEqual(result["status"], "unknown", result)
        self.assertEqual(result["reason"], reason, result)

    def test_exact_host_admission_preserves_display_and_excludes_trigger(self):
        rows = sequence()
        result = self.qualify(rows)
        self.assertEqual(result["status"], "qualified")
        self.assertEqual(result["accepted"]["rendered_text"], TEXT)
        self.assertEqual(result["accepted"]["ref"]["ordinal"], 4)
        self.assertEqual(result["accepted"]["ref"]["raw_line_sha256"],
                         evidence.digest(encoded([rows[4]])))
        self.assertEqual([a["call_id"] for a in result["actions"]], ["later"])
        self.assertEqual(result["actions"][0]["ref"]["ordinal"], 6)
        self.assertEqual(result["actions"][0]["result"]["ref"]["ordinal"], 7)
        self.assertEqual(result["ignored_pre_admission_results"], 1)
        self.assertEqual(result["binding"]["caller_binding"], packet()["caller_binding"])
        self.assertNotIn("helped", result)

    def test_packet_identity_and_display_bytes_are_not_coerced(self):
        for field, value, reason in [
            ("rendered_sha256", "0" * 64, "packet_digest_mismatch"),
            ("rendered_text", TEXT.rstrip(), "packet_digest_mismatch"),
            ("rendered_text", "\ud800", "invalid_packet"),
            ("session_id", True, "invalid_packet"),
            ("turn_id", "", "invalid_packet"),
            ("caller_binding", ["not an object"], "invalid_caller_binding"),
        ]:
            with self.subTest(field=field, value=repr(value)):
                p = packet(); p[field] = value
                self.unknown(self.qualify(emission=p), reason)
        self.unknown(self.qualify(emission=packet("x" * 8193)), "packet_bytes_exceeded")

    def test_same_id_different_summary_or_unseen_body_does_not_match(self):
        rows = sequence(); rows[4] = admission("changed summary for the same card-1")
        self.unknown(self.qualify(rows), "packet_not_found")
        result = self.qualify()
        self.assertNotIn("body", result["accepted"])
        self.assertNotIn("native_routes", result)

    def test_host_role_and_metadata_are_required_not_text_markers(self):
        rows = sequence(); rows[4]["payload"]["role"] = "user"
        self.unknown(self.qualify(rows), "packet_not_developer_input")
        for alteration, reason in [
            (lambda p: p.pop(evidence.META), "packet_turn_mismatch"),
            (lambda p: p[evidence.META].pop("content_item_kinds"), "missing_or_ambiguous_hook_metadata"),
            (lambda p: p[evidence.META].update(content_item_kinds=["ordinary"]), "packet_not_hook_input"),
            (lambda p: p.update(id=None), "missing_admission_item_id"),
        ]:
            rows = sequence(); alteration(rows[4]["payload"])
            self.unknown(self.qualify(rows), reason)
        rows = sequence(); rows[4]["metadata"] = rows[4]["payload"].pop(evidence.META)
        self.unknown(self.qualify(rows), "packet_turn_mismatch")

    def test_multi_content_matches_only_the_corresponding_hook_slot(self):
        rows = sequence(); p = rows[4]["payload"]
        p["content"].insert(0, {"type": "input_text", "text": "ordinary developer content"})
        p[evidence.META]["content_item_kinds"].insert(0, "ordinary")
        result = self.qualify(rows)
        self.assertEqual(result["status"], "qualified")
        self.assertEqual(result["accepted"]["content_index"], 1)
        p[evidence.META]["content_item_kinds"] = ["hooks.additional_context", "ordinary"]
        self.unknown(self.qualify(rows), "packet_not_hook_input")
        p[evidence.META]["content_item_kinds"] = ["hooks.additional_context"]
        self.unknown(self.qualify(rows), "missing_or_ambiguous_hook_metadata")

    def test_fragmented_or_repeated_content_cannot_manufacture_packet(self):
        rows = sequence(); p = rows[4]["payload"]
        p["content"] = [{"type": "input_text", "text": TEXT[:10]},
                        {"type": "input_text", "text": TEXT[10:]}]
        p[evidence.META]["content_item_kinds"] *= 2
        self.unknown(self.qualify(rows), "packet_not_found")
        p["content"] = [{"type": "input_text", "text": TEXT}] * 2
        self.unknown(self.qualify(rows), "ambiguous_packet_slots")

    def test_session_and_source_turn_are_structured_bindings(self):
        rows = sequence(); rows[0]["payload"]["id"] = "wrong-session"
        self.unknown(self.qualify(rows), "session_mismatch")
        rows = sequence(); rows[4] = admission(turn="other-turn")
        self.unknown(self.qualify(rows), "packet_turn_mismatch")
        rows = sequence(); rows.insert(4, event("task_started", "other-turn"))
        result = self.qualify(rows)
        self.unknown(result, "turn_changed")
        self.assertIsNone(result["accepted"])
        rows = sequence(); rows.insert(6, row("turn_context", {"turn_id": "other-turn"}))
        result = self.qualify(rows)
        self.unknown(result, "turn_changed")
        self.assertIsNotNone(result["accepted"])
        self.assertEqual(result["actions"], [])

    def test_cancellation_compaction_and_restart_do_not_reopen_task(self):
        for fence, reason in [
            (event("turn_aborted"), "source_task_interrupted"),
            (row("compacted", {}), "source_task_interrupted"),
            (row("session_meta", {"id": SESSION}), "repeated_session_metadata"),
            (event("task_started"), "repeated_task_start"),
        ]:
            with self.subTest(reason=reason):
                rows = sequence(); rows.insert(6, fence)
                result = self.qualify(rows)
                self.unknown(result, reason)
                self.assertEqual(result["actions"], [])

    def test_multiple_admissions_are_not_selected_by_favorable_order(self):
        rows = sequence(); extra = admission(); extra["payload"]["id"] = "admission-2"
        rows.insert(6, extra)
        self.unknown(self.qualify(rows), "duplicate_admission")

    def test_repeated_call_or_result_and_wrong_pair_type_are_unknown(self):
        rows = sequence(); rows[6] = call("trigger")
        rows[6]["payload"]["id"] = "distinct-item-id"
        self.unknown(self.qualify(rows), "duplicate_call_id")
        rows = sequence(); rows.insert(8, output()); rows[8]["payload"]["id"] = "another-output"
        self.unknown(self.qualify(rows), "duplicate_result")
        rows = sequence(); rows[7] = output(typ="function_call_output")
        self.unknown(self.qualify(rows), "result_type_mismatch")
        rows = sequence(); rows[7] = output("orphan")
        self.unknown(self.qualify(rows), "result_without_start")

    def test_function_pairs_supported_without_parsing_or_executing_arguments(self):
        rows = prefix() + [admission(), call(typ="function_call", text='{"cmd":"false"}'),
                           output(typ="function_call_output", text={"exit_code": 1}), event("task_complete")]
        result = self.qualify(rows)
        self.assertEqual(result["status"], "qualified")
        self.assertEqual(result["actions"][0]["input"], '{"cmd":"false"}')
        self.assertEqual(result["actions"][0]["result"]["output"], {"exit_code": 1})
        self.assertNotIn("success", result)

    def test_missing_action_metadata_or_result_remains_incomplete(self):
        rows = sequence(); del rows[6]["payload"][evidence.META]
        self.unknown(self.qualify(rows), "action_turn_metadata_mismatch")
        rows = sequence(); rows.pop(7)
        result = self.qualify(rows)
        self.unknown(result, "action_result_missing")
        self.assertIsNone(result["actions"][0]["result"])
        self.assertTrue(result["partial"])

    def test_complete_turn_with_no_post_admission_actions_proves_no_use(self):
        rows = prefix() + [admission(), event("task_complete")]
        result = self.qualify(rows)
        self.assertEqual(result["status"], "qualified")
        self.assertEqual(result["actions"], [])
        self.assertEqual(result["outcome_evidence"], "absent")

    def test_no_tool_public_answer_and_commentary_without_private_reasoning(self):
        rows = prefix() + [public_message("before", text="Pre-admission output"), admission(),
                           public_message("note", phase="commentary", text="Visible progress\nline 2"),
                           public_message("private", phase="analysis", text="PRIVATE_ANALYSIS"),
                           row("response_item", {"id": "reasoning-1", "type": "reasoning",
                                                "content": "PRIVATE_REASONING"}),
                           public_message(), event("task_complete")]
        result = self.qualify(rows)
        self.assertEqual(result["status"], "qualified")
        self.assertEqual(result["actions"], [])
        self.assertEqual(result["outcome_evidence"], "observed")
        self.assertEqual([p["phase"] for p in result["public_outputs"]], ["commentary", "final_answer"])
        self.assertEqual(result["public_outputs"][0]["content"][0]["text"], "Visible progress\nline 2")
        self.assertNotIn("PRIVATE", json.dumps(result))
        self.assertNotIn("Pre-admission output", json.dumps(result["public_outputs"]))
        prior = result["prior_or_inflight_context"]["items"]
        self.assertEqual(prior[0]["content"][0]["text"], "Pre-admission output")
        self.assertFalse(prior[0]["creditable"])

    def test_preknown_prompt_duplicate_is_retained_without_becoming_a_target(self):
        rows = prefix() + [task_prompt(TEXT), admission(), public_message(), event("task_complete")]
        result = self.qualify(rows)
        self.assertEqual(result["status"], "qualified")
        prior = result["prior_or_inflight_context"]
        self.assertEqual(prior["coverage"]["task_prompt"], "observed")
        self.assertEqual(prior["items"][0]["content"][0]["text"], TEXT)
        self.assertFalse(prior["items"][0]["creditable"])
        self.assertEqual(result["actions"], [])
        self.assertEqual(len(result["public_outputs"]), 1)
        self.assertEqual(prior["coverage"]["earlier_session_context"], "omitted")

    def test_inflight_read_result_remains_visible_but_not_card_credited(self):
        rows = prefix() + [task_prompt(), call("read", text="cat(config)"), admission(),
                           output("read", text="target=R5"), call("apply"), output("apply"),
                           event("task_complete")]
        result = self.qualify(rows)
        self.assertEqual(result["status"], "qualified")
        self.assertEqual([a["call_id"] for a in result["actions"]], ["apply"])
        prior = result["prior_or_inflight_context"]
        independent = prior["items"][1]
        self.assertEqual(independent["input"], "cat(config)")
        self.assertFalse(independent["creditable"])
        self.assertLess(independent["ref"]["ordinal"], result["accepted"]["ref"]["ordinal"])
        self.assertEqual(independent["result"]["output"], "target=R5")
        self.assertEqual(independent["result"]["timing"], "after_admission")
        self.assertGreater(independent["result"]["ref"]["ordinal"], result["accepted"]["ref"]["ordinal"])
        self.assertEqual(result["ignored_pre_admission_results"], 1)
        self.assertEqual(prior["coverage"]["prior_results"], "observed")

    def test_prior_tool_observation_and_public_plan_are_noncreditable(self):
        rows = prefix() + [task_prompt(), call("read"), output("read", text="already known R5"),
                           public_message("plan", phase="commentary", text="I will use the R5 path."),
                           admission(), public_message(), event("task_complete")]
        result = self.qualify(rows)
        prior = result["prior_or_inflight_context"]["items"]
        self.assertEqual(result["status"], "qualified")
        self.assertEqual([p["kind"] for p in prior], ["task_prompt", "tool_call", "assistant_public_message"])
        self.assertTrue(all(p["creditable"] is False for p in prior))
        self.assertEqual(prior[1]["result"]["timing"], "before_admission")
        self.assertEqual(prior[2]["content"][0]["text"], "I will use the R5 path.")
        self.assertEqual(result["actions"], [])

    def test_missing_and_oversized_prompt_do_not_revoke_admission(self):
        result = self.qualify()
        self.assertEqual(result["status"], "qualified")
        self.assertEqual(result["prior_or_inflight_context"]["coverage"]["task_prompt"], "missing")
        rows = prefix() + [task_prompt("x" * 8193), admission(), event("task_complete")]
        result = self.qualify(rows)
        self.assertEqual(result["status"], "qualified")
        prior = result["prior_or_inflight_context"]
        self.assertEqual(prior["coverage"]["task_prompt"], "too_large")
        self.assertTrue(prior["coverage"]["truncated"])
        self.assertEqual(prior["items"], [])

    def test_prior_item_and_byte_caps_are_coverage_limits_not_failed_admission(self):
        rows = prefix() + [task_prompt(), call("read"), admission(), output("read"), event("task_complete")]
        for limits in (evidence.Limits(prior_items=1), evidence.Limits(prior_context_bytes=1)):
            result = self.qualify(rows, limits=limits)
            self.assertEqual(result["status"], "qualified")
            prior = result["prior_or_inflight_context"]
            self.assertLessEqual(prior["bytes"], limits.prior_context_bytes)
            self.assertLessEqual(len(prior["items"]), limits.prior_items)
            self.assertTrue(prior["coverage"]["truncated"])
            self.assertEqual(prior["coverage"]["prior_results"], "partial")

    def test_missing_or_oversized_inflight_result_only_degrades_context(self):
        rows = prefix() + [call("read"), admission(), event("task_complete")]
        result = self.qualify(rows)
        self.assertEqual(result["status"], "qualified")
        self.assertEqual(result["prior_or_inflight_context"]["coverage"]["missing_results"], 1)
        rows.insert(5, output("read", text="x" * 17000))
        result = self.qualify(rows)
        self.assertEqual(result["status"], "qualified")
        self.assertEqual(result["prior_or_inflight_context"]["coverage"]["missing_results"], 0)
        self.assertEqual(result["prior_or_inflight_context"]["coverage"]["prior_results"], "partial")
        self.assertIsNone(result["prior_or_inflight_context"]["items"][0]["result"])

    def test_invalid_prior_result_never_claims_observed_result_coverage(self):
        for invalid in (output("read", typ="function_call_output"), output("read")):
            if invalid["payload"]["type"] == "custom_tool_call_output":
                del invalid["payload"]["output"]
            rows = prefix() + [call("read"), admission(), invalid, event("task_complete")]
            result = self.qualify(rows)
            self.unknown(result, "result_type_mismatch")
            coverage = result["prior_or_inflight_context"]["coverage"]
            self.assertEqual(coverage["missing_results"], 1)
            self.assertEqual(coverage["prior_results"], "missing")

    def test_task_start_after_admission_cannot_repair_missing_start_boundary(self):
        rows = [row("session_meta", {"id": SESSION}), row("turn_context", {"turn_id": TURN}),
                admission(), event("task_started"), event("task_complete")]
        result = self.qualify(rows)
        self.unknown(result, "missing_task_start")
        self.assertIsNotNone(result["accepted"])
        coverage = result["prior_or_inflight_context"]["coverage"]
        self.assertEqual(coverage["source_turn_prefix"], "incomplete")
        self.assertIn("task_start_not_observed_before_admission", coverage["omissions"])

    def test_only_same_turn_user_text_prompt_not_environment_or_projection(self):
        wrapper = task_prompt("ENVIRONMENT_NOT_TASK", identifier="environment")
        wrapper["payload"][evidence.META]["content_item_kinds"] = ["environments.environment_context"]
        projection = row("event_msg", {"type": "item_completed", "turn_id": TURN,
            "item": {"type": "UserMessage", "text": "PROJECTION_NOT_ADDITIONAL_INPUT"}})
        foreign = task_prompt("OTHER_TURN_NOT_TASK", identifier="foreign", turn="old-turn")
        rows = prefix() + [wrapper, foreign, task_prompt(), projection, admission(), event("task_complete")]
        result = self.qualify(rows)
        self.assertEqual(result["status"], "qualified")
        prior = result["prior_or_inflight_context"]
        self.assertEqual([i["kind"] for i in prior["items"]], ["task_prompt"])
        for omitted in ("ENVIRONMENT_NOT_TASK", "PROJECTION_NOT_ADDITIONAL_INPUT", "OTHER_TURN_NOT_TASK"):
            self.assertNotIn(omitted, json.dumps(result))
        self.assertIn("user_input_metadata_unavailable", prior["coverage"]["omissions"])

    def test_prior_lane_never_exports_analysis_or_private_reasoning(self):
        rows = prefix() + [public_message("private", phase="analysis", text="PRIVATE_ANALYSIS"),
                           row("response_item", {"id": "private-r", "type": "reasoning",
                                                "content": "PRIVATE_REASONING"}),
                           task_prompt(), admission(), event("task_complete")]
        result = self.qualify(rows)
        self.assertEqual(result["status"], "qualified")
        self.assertNotIn("PRIVATE", json.dumps(result))
        self.assertEqual(len(result["prior_or_inflight_context"]["items"]), 1)

    def test_unsupported_prior_action_marks_coverage_but_later_action_stays_unknown(self):
        unsupported = row("response_item", {"id": "web-1", "type": "web_search_call",
            "action": {"query": "independent fact"}, evidence.META: {"turn_id": TURN}})
        rows = prefix() + [unsupported, admission(), event("task_complete")]
        result = self.qualify(rows)
        self.assertEqual(result["status"], "qualified")
        prior = result["prior_or_inflight_context"]
        self.assertEqual(prior["coverage"]["omissions"], {"unsupported_prior_action": 1})
        self.assertEqual(prior["coverage"]["source_turn_prefix"], "incomplete")
        self.assertNotIn("independent fact", json.dumps(result))
        rows = prefix() + [admission(), unsupported, event("task_complete")]
        self.unknown(self.qualify(rows), "unsupported_action_type")

    def test_earlier_turn_public_inputs_and_calls_do_not_enter_source_context(self):
        rows = [row("session_meta", {"id": SESSION}), event("task_started", "old-turn"),
                task_prompt("OLD_TURN_FACT", identifier="old-prompt", turn="old-turn"),
                call("old", turn="old-turn", text="OLD_TURN_READ"),
                output("old", turn="old-turn", text="OLD_TURN_RESULT"),
                event("task_complete", "old-turn"), event("task_started"),
                task_prompt(), admission(), event("task_complete")]
        result = self.qualify(rows)
        self.assertEqual(result["status"], "qualified")
        self.assertNotIn("OLD_TURN", json.dumps(result))
        prior = result["prior_or_inflight_context"]
        self.assertEqual(len(prior["items"]), 1)
        self.assertEqual(prior["coverage"]["earlier_session_context"], "omitted")

    def test_core_evidence_can_displace_optional_prior_context_with_honest_coverage(self):
        rows = prefix() + [task_prompt("x" * 700), admission(), public_message(text="y" * 700),
                           event("task_complete")]
        result = self.qualify(rows, limits=evidence.Limits(evidence_bytes=1800))
        self.assertEqual(result["status"], "qualified")
        prior = result["prior_or_inflight_context"]
        self.assertEqual(prior["items"], [])
        self.assertEqual(prior["coverage"]["task_prompt"], "omitted_budget")
        self.assertIn("prior_context_displaced", prior["coverage"]["omissions"])

    def test_public_output_requires_turn_and_bounded_known_text_shape(self):
        rows = prefix() + [admission(), public_message(turn="different"), event("task_complete")]
        self.unknown(self.qualify(rows), "public_output_turn_metadata_mismatch")
        rows[4] = public_message(); rows[4]["payload"]["content"] = [{"type": "image", "data": "not text"}]
        self.unknown(self.qualify(rows), "unsupported_public_output_content")
        rows[4] = public_message(); rows.insert(5, public_message("another"))
        self.unknown(self.qualify(rows, limits=evidence.Limits(public_outputs=1)), "public_output_limit_exceeded")

    def test_missing_start_conflicting_session_and_duplicate_generic_item_are_unknown(self):
        rows = sequence(); rows.pop(1)
        self.unknown(self.qualify(rows), "missing_task_start")
        rows = sequence(); rows[0]["payload"]["session_id"] = "other"
        self.unknown(self.qualify(rows), "session_mismatch")
        rows = sequence(); rows.insert(6, public_message("admission-1"))
        self.unknown(self.qualify(rows), "repeated_response_item_id")

    def test_bad_response_type_or_unencodable_action_returns_unknown_not_exception(self):
        for value in (None, [], {}):
            rows = sequence(); rows[6]["payload"]["type"] = value
            self.unknown(self.qualify(rows), "malformed_record")
        rows = sequence(); rows[6]["payload"]["input"] = "\ud800"
        raw = b"".join(json.dumps(r).encode() + b"\n" for r in rows)
        self.unknown(evidence.qualify(packet(), raw), "malformed_action")

    def test_nested_valid_output_does_not_expand_cli_pretty_printing(self):
        nested = [0] * 1000
        for _ in range(100):
            nested = [nested]
        rows = sequence(); rows[7]["payload"]["output"] = nested
        with tempfile.TemporaryDirectory() as directory:
            p = Path(directory) / "packet.json"; r = Path(directory) / "rollout.jsonl"
            p.write_text(json.dumps(packet())); r.write_bytes(encoded(rows))
            with contextlib.redirect_stdout(io.StringIO()) as stdout:
                self.assertEqual(evidence.main(["--packet", str(p), "--rollout", str(r)]), 0)
            self.assertLess(len(stdout.getvalue().encode()), evidence.MAX_OUTPUT_BYTES)

    def test_truncated_unflushed_malformed_or_duplicate_key_records(self):
        raw = encoded(sequence())
        self.unknown(evidence.qualify(packet(), raw[:-1]), "unflushed_record")
        self.unknown(evidence.qualify(packet(), raw[:-20]), "unflushed_record")
        self.unknown(evidence.qualify(packet(), encoded(prefix()) + b'{bad}\n'), "malformed_record")
        self.unknown(evidence.qualify(packet(), b'{"type":"x","type":"session_meta","payload":{}}\n'),
                     "malformed_record")
        self.unknown(evidence.qualify(packet(), b''), "missing_session_metadata")
        self.unknown(evidence.qualify(packet(), encoded(sequence()[1:])), "missing_session_metadata")
        self.unknown(self.qualify(sequence()[:-1]), "source_turn_not_closed")

    def test_each_resource_bound_fails_honestly_with_no_truncated_evidence(self):
        for limits, reason in [
            (evidence.Limits(scan_bytes=len(encoded(sequence())) - 1), "scan_bytes_exceeded"),
            (evidence.Limits(events=6), "event_limit_exceeded"),
            (evidence.Limits(line_bytes=50), "line_bytes_exceeded"),
            (evidence.Limits(packet_bytes=4), "packet_bytes_exceeded"),
            (evidence.Limits(item_evidence_bytes=20), "evidence_bytes_exceeded"),
            (evidence.Limits(evidence_bytes=20), "evidence_bytes_exceeded"),
            (evidence.Limits(events=True), "invalid_limits"),
        ]:
            with self.subTest(limits=limits):
                self.unknown(self.qualify(limits=limits), reason)
        rows = sequence(); rows[8:8] = [call("second"), output("second")]
        self.unknown(self.qualify(rows, limits=evidence.Limits(actions=1)), "action_limit_exceeded")

    def test_cli_only_reads_explicit_files_and_has_machine_readable_failure(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory); p = root / "emission.json"; r = root / "rollout.jsonl"
            p.write_text(json.dumps(packet())); r.write_bytes(encoded(sequence()))
            before = {f.name: f.read_bytes() for f in root.iterdir()}
            with contextlib.redirect_stdout(io.StringIO()) as stdout:
                status = evidence.main(["--packet", str(p), "--rollout", str(r)])
            self.assertEqual(status, 0)
            self.assertEqual(json.loads(stdout.getvalue())["status"], "qualified")
            self.assertEqual(before, {f.name: f.read_bytes() for f in root.iterdir()})
            with contextlib.redirect_stdout(io.StringIO()) as stdout:
                status = evidence.main(["--packet", str(root / "missing"), "--rollout", str(r)])
            self.assertEqual(status, 2)
            self.assertEqual(json.loads(stdout.getvalue())["status"], "unknown")
            link = root / "link"; link.symlink_to(r)
            self.assertEqual(evidence.qualify_files(p, link)["status"], "unknown")



if __name__ == "__main__":
    unittest.main()
