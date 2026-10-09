"""Pure authored-envelope and authority checks, without provider calls."""
import copy
import json
import unittest

import stewardship_contract as contract
from librarian_policy import LibrarianBudget
from tag_context import TagContext
from fixture_stewardship import target, context, DB, A, B


class ContractTests(unittest.TestCase):
    def prepare(self,targets=None,ctx=None,budget=None):
        return contract.prepare(targets or [target()],ctx or context(),budget=budget or LibrarianBudget())

    def test_prospective_context_and_complete_summary_without_body(self):
        node=target();node["summary"]="supported semantic detail "*130
        prompt,ctx=self.prepare([node])
        self.assertIsNotNone(prompt)
        self.assertEqual(json.loads(prompt)["targets"][0]["summary"],node["summary"])
        self.assertNotIn("content_fingerprint",prompt)
        self.assertIn("future questions",contract.instructions(ctx))
        self.assertIn("historical accounts",contract.instructions(ctx))

    def test_complete_batch_no_unknown_duplicate_missing_or_extra(self):
        _,ctx=self.prepare([target(),target(B)])
        valid={"decisions":[{"id":n["id"],"disposition":"noop","tags":n["tags"]} for n in ctx.targets]}
        self.assertEqual(contract.validate_answer(valid,ctx),valid)
        invalid=[{"decisions":valid["decisions"][:1]},
            {"decisions":[valid["decisions"][0]]*2},
            {"decisions":valid["decisions"],"reason":"not admitted"}]
        for value in invalid:
            with self.assertRaises(ValueError):contract.validate_answer(value,ctx)

    def test_protected_special_and_namespaced_tags_preserved(self):
        node=target();node["tags"]=["rust","core","identity:person-42","closed","possibility"]
        _,ctx=self.prepare([node])
        for tags in (["rust"],[*node["tags"],"pursuing"],["rust","core","identity:person-43","closed","possibility"]):
            with self.assertRaises(ValueError):contract.validate_answer({"decisions":[{"id":A,"disposition":"retag","tags":tags}]},ctx)
        result=contract.validate_answer({"decisions":[{"id":A,"disposition":"retag","tags":[*node["tags"],"people"]}]},ctx)
        self.assertIn("people",result["decisions"][0]["tags"])

    def test_noop_and_unresolved_cannot_smuggle_mutations(self):
        _,ctx=self.prepare()
        for disposition in ("noop","unresolved"):
            with self.assertRaises(ValueError):contract.validate_answer({"decisions":[{"id":A,"disposition":disposition,"tags":["people"]}]},ctx)
        with self.assertRaises(ValueError):contract.validate_answer({"decisions":[{"id":A,"disposition":"retag","tags":["rust"]}]},ctx)

    def test_native_64_tag_preservation_uses_actual_answer_bytes(self):
        node=target();node["tags"]=["tag-%02d"%i for i in range(64)]
        _,ctx=self.prepare([node])
        result=contract.validate_answer({"decisions":[{"id":A,"disposition":"noop","tags":node["tags"]}]},ctx)
        self.assertEqual(len(result["decisions"][0]["tags"]),64)
        node["tags"].append("tag-64")
        self.assertIsNone(self.prepare([node])[0])

    def test_huge_summary_never_silently_truncated(self):
        node=target();node["summary"]="x"*16000
        self.assertIsNone(self.prepare([node],budget=LibrarianBudget(effort="low"))[0])
        prompt,_=self.prepare([node],budget=LibrarianBudget(effort="high"))
        self.assertEqual(json.loads(prompt)["targets"][0]["summary"],node["summary"])

    def test_optional_vocabulary_omission_is_explicit(self):
        items=[{"name":"topic-"+str(i)+"x"*110,"count":{"status":"unavailable"},"examples":[]} for i in range(40)]
        ctx=TagContext(True,"partial",DB,None,json.dumps({"items":items,"partial":False,"coverage":{}}))
        prompt,_=self.prepare(ctx=ctx,budget=LibrarianBudget(effort="low"))
        packet=json.loads(prompt)["tag_context"]["vocabulary"]
        self.assertTrue(packet["partial"])
        self.assertGreater(packet["coverage"]["model_omitted_items"],0)
        self.assertEqual(len(ctx.vocabulary["items"]),40)

    def test_malformed_target_and_disabled_context_do_not_invoke(self):
        for change in ({"body":"hidden"},{"summary":""},{"content_fingerprint":"wrong"},{"tags":["duplicate","duplicate"]}):
            self.assertIsNone(self.prepare([{**target(),**change}])[0])
        ctx=context();ctx=TagContext(False,ctx.outcome,ctx.db_id,ctx.guide_json,ctx.vocabulary_json)
        self.assertIsNone(self.prepare(ctx=ctx)[0])


if __name__=="__main__":unittest.main()
