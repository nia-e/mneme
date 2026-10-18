"""Provider-independent prepared request, policy and strict validation tests."""
from dataclasses import FrozenInstanceError
import math
from pathlib import Path
import threading
import time
import unittest
from unittest.mock import patch

from assessment import (AssessmentMixin, AssessmentRequest, AssessmentResponse, TransportError,
                        MAX_PREPARED_BYTES, runtime, validate_response)
from librarian_policy import LibrarianBudget, MODEL, resolve


class FixtureRuntime(AssessmentMixin):
    def __init__(self, answer='{"proposal":null}'):
        self.budget=LibrarianBudget();self._lock=threading.Lock()
        self._closed=self._halted=self.attempt=False
        self._current_usage=None
        self.answer=answer;self.requests=[]
    def _begin_assessment(self,_frame):
        self.attempt=False;self._current_usage=None
    def _assessment_attempted(self):return self.attempt
    def _run_assessment(self,request,*,service_name):
        self.attempt=True;self.requests.append((request,service_name))
        return AssessmentResponse(self.answer,{"input_tokens":10,"output_tokens":2},MODEL)
    def _stop_process(self):pass
    def _finish_assessment(self,*_):pass


class AssessmentTests(unittest.TestCase):
    def request(self,**updates):
        values={"instructions":"rules","prompt":"input","output_schema":{"type":"object"},
                "answer_byte_limit":100,"deadline":time.monotonic()+1}
        values.update(updates);return AssessmentRequest(**values)

    def test_narrow_request_is_checked_and_frozen(self):
        request=self.request()
        with self.assertRaises(FrozenInstanceError): request.prompt="other"
        self.assertIn('OUTPUT_SCHEMA:\n{"type":"object"}',request.prompt_with_schema())
        self.assertTrue(request.prompt_with_schema().endswith("ASSESSMENT_INPUT:\ninput"))
        for overrides in ({"instructions":None},{"prompt":[]},{"output_schema":[]},
                          {"answer_byte_limit":True},{"answer_byte_limit":0},{"deadline":math.inf},
                          {"output_schema":{"bad":float('nan')}},{"prompt":"x"*MAX_PREPARED_BYTES}):
            with self.subTest(overrides=list(overrides)),self.assertRaises(TransportError):self.request(**overrides)

    def test_schema_overhead_is_charged_to_same_input_envelope(self):
        request=self.request(prompt="x"*1000)
        # The exact same prepared byte bound applies when Codex transports the
        # schema as outputSchema and Claude transports it as prompt data.
        overhead=len(request.prompt_with_schema().encode())-1000+len(request.instructions.encode())
        self.request(prompt="x"*(MAX_PREPARED_BYTES-overhead))
        with self.assertRaisesRegex(TransportError,"authored_input_limit"):
            self.request(prompt="x"*(MAX_PREPARED_BYTES-overhead+1))

    def test_strict_json_once_and_terminal_role_validation(self):
        calls=[]
        validator=lambda value,context:(calls.append((value,context)),value)[1]
        self.assertEqual(validate_response(AssessmentResponse('{"ok":1}',{},MODEL),self.request(),"context",validator),{"ok":1})
        self.assertEqual(calls,[({"ok":1},"context")])
        for answer in ('{"ok":1,"ok":2}','{"ok":NaN}','```json\n{}\n```', 'x'*101):
            with self.subTest(answer=answer),self.assertRaises(TransportError):
                validate_response(AssessmentResponse(answer,{},MODEL),self.request(),{},validator)
        self.assertEqual(len(calls),1)

    def test_assess_preparation_contract_validation_and_projection_are_shared(self):
        fixture=FixtureRuntime();context=object()
        with (patch("recording_contract.prepare",return_value=("prepared",context)) as prepare,
              patch("recording_contract.instructions_for",return_value="recording rules"),
              patch("recording_contract.schema_for",return_value={"type":"object"}),
              patch("recording_contract.validate_answer",return_value={"proposal":None,"maintenance":[],
                    "omissions":{"proposal":"invalid_proposal_shape"}}) as validate):
            result=fixture.assess({"observed":"data"},selected_stores=[])
        self.assertEqual(result["reason"],"validation_failed")
        self.assertEqual(result["intent_omissions"],{"proposal":"invalid_proposal_shape"})
        self.assertEqual(validate.call_args.args,({"proposal":None},context))
        self.assertEqual(prepare.call_args.kwargs["selected_stores"],[])
        request,role=fixture.requests[0]
        self.assertEqual((request.instructions,request.prompt,role),("recording rules","prepared","mneme_recorder"))

    def test_route_and_steward_use_the_same_narrow_request_and_validator(self):
        fixture=FixtureRuntime('{"ok":true}')
        from types import SimpleNamespace
        context=object()
        with (patch("routing_contract.prepare",return_value=("route input",context)),
              patch("routing_contract.schema",return_value={"type":"object"}),
              patch("routing_contract.validate_answer",return_value=SimpleNamespace(hints=[])) as validate):
            result=fixture.route([],[],expected_db_id="db")
        self.assertEqual(result["reason"],"neutral");self.assertEqual(validate.call_args.args,({"ok":True},context))
        with (patch("stewardship_contract.prepare",return_value=("steward input",context)),
              patch("stewardship_contract.instructions",return_value="steward rules"),
              patch("stewardship_contract.schema",return_value={"type":"object"}),
              patch("stewardship_contract.validate_answer",return_value={"decisions":[]}) as validate):
            result=fixture.steward([], {})
        self.assertEqual(result["reason"],"classified");self.assertEqual(result["decisions"],[])
        self.assertEqual(validate.call_args.args,({"ok":True},context))
        self.assertEqual([role for _,role in fixture.requests],["mneme_router","mneme_tag_steward"])

    def test_explicit_claude_policy_keeps_provider_independent_work_presets(self):
        for effort in ("low","medium","high"):
            codex=resolve({"reader_model":MODEL,"librarian_effort":effort})
            claude=resolve({"assessor":"claude_code","reader_model":"claude-sonnet-4-5-20250929","librarian_effort":effort})
            for field in ("attempts","input_tokens","output_tokens","native_read_bytes","recording_hint_bytes"):
                self.assertEqual(getattr(codex,field),getattr(claude,field))
        for model in ("sonnet","opus","claude-sonnet","claude-sonnet-latest",MODEL,None,True):
            with self.subTest(model=model),self.assertRaises(ValueError):
                resolve({"assessor":"claude_code","reader_model":model,"librarian_effort":"medium"})
        with self.assertRaises(ValueError):resolve({"reader_model":"claude-opus-4-6","librarian_effort":"medium"})
        with self.assertRaises(ValueError):resolve({"assessor":"future","reader_model":MODEL,"librarian_effort":"medium"})
        with self.assertRaises(ValueError):runtime({"assessor":"future"},Path("unused"))

    def test_claude_factory_does_not_import_or_construct_codex_runtime(self):
        import builtins
        original=builtins.__import__
        def guarded(name,*args,**kwargs):
            if name=="reader_runtime":raise AssertionError("Claude must not depend on Codex runtime")
            return original(name,*args,**kwargs)
        with patch("builtins.__import__",side_effect=guarded):
            result=runtime({"assessor":"claude_code","reader_model":"claude-opus-4-6","librarian_effort":"high"},Path("unused"))
        self.assertEqual(type(result).__name__,"ClaudeRuntime");result.close()

if __name__=="__main__":unittest.main()
