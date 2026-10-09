"""Provider-free guidance/vocabulary admission and bounded lookup tests."""
from __future__ import annotations

from dataclasses import replace
import hashlib
import json
from pathlib import Path
import sys
from types import SimpleNamespace
import unittest
from unittest.mock import patch

sys.path.insert(0, str(Path(__file__).parent))
import tag_context as tags

DB = "00000000000000000000000001"
GUIDE = "00000000000000000000000002"
OTHER = "00000000000000000000000003"


def vocabulary(names=(), *, partial=False):
    return {"items": [{"name": name, "count": {"status": "exact", "value": 1},
                       "examples": [OTHER]} for name in names],
            "partial": partial, "coverage": {"semantic_only": True, "snapshot": False}}


def context(names=(), *, guide=None):
    return tags.TagContext(True, "ok", DB, tags.encoded(guide).decode() if guide else None,
                           tags.encoded(vocabulary(names)).decode())


def guide(text="Preserve people and roles across time."):
    return {"id": GUIDE, "content_fingerprint": "a" * 64,
            "text_sha256": hashlib.sha256(text.encode()).hexdigest(), "text": text}


class TagContextTests(unittest.TestCase):
    def test_ordinary_names_are_not_ontology_and_special_tags_are_host_owned(self):
        self.assertEqual(tags.ordinary_tags(["rust", "People", "new-concept"]),
                         ("People", "new-concept", "rust"))
        for value in (["core"], ["possibility"], ["routing-judgment"], ["collaboration-preference"],
                      ["rust", "rust"], [" bad"], ["x\x85y"], ["é" * 129], "rust", [f"t{i}" for i in range(65)]):
            with self.subTest(value=value), self.assertRaises(ValueError):
                tags.ordinary_tags(value)

    def test_native_valid_long_identity_names_and_sixty_four_seeds_are_preserved(self):
        names = [f"identity-{i}" for i in range(63)] + ["rare-" + "é" * 90]
        self.assertEqual(set(tags.ordinary_tags(names)), set(names))
        self.assertEqual(tags.lookup_prefixes("", names), names)
        self.assertTrue(tags.validate_context(context([names[-1]])).enabled)

    def test_guide_revision_is_not_observed_vocabulary_growth(self):
        first = context(["people"], guide=guide())
        second = context(["people", "rust"], guide=guide())
        self.assertEqual(first.snapshot()["guide_revision_sha256"], second.snapshot()["guide_revision_sha256"])
        self.assertNotEqual(first.snapshot()["vocabulary_sha256"], second.snapshot()["vocabulary_sha256"])
        changed = context(["people"], guide={**guide(), "content_fingerprint": "b" * 64})
        self.assertEqual(first.snapshot()["guide_revision_sha256"], changed.snapshot()["guide_revision_sha256"])
        changed = context(["people"], guide=guide("Changed policy, not background body."))
        self.assertNotEqual(first.snapshot()["guide_revision_sha256"], changed.snapshot()["guide_revision_sha256"])
        self.assertEqual(first.content_guards, [{"id": GUIDE, "content_fingerprint": "a" * 64}])
        exposed = first.guide
        exposed["text"] = "not shared mutable policy"
        self.assertEqual(first.guide["text"], guide()["text"])

    def test_wrong_owner_bad_body_and_partial_malformed_vocabulary(self):
        with self.assertRaises(ValueError):
            tags.validate_context(context(), OTHER)
        for changed in ({**guide(), "text": "changed bytes"}, {**guide(), "text": "\x00"},
                        {**guide(), "id": "not-an-id"}):
            with self.assertRaises(ValueError):
                tags.validate_context(context(guide=changed), DB)
        malformed = vocabulary(["people"])
        malformed["items"][0]["count"] = {"status": "exact", "value": True}
        with self.assertRaises(ValueError):
            tags.validate_context(replace(context(), vocabulary_json=json.dumps(malformed)))
        self.assertTrue(tags.validate_context(context(["core"])).enabled)
        partial = replace(context(), vocabulary_json=json.dumps(vocabulary(["people"], partial=True)))
        self.assertTrue(tags.validate_context(partial).snapshot()["vocabulary_partial"])

    def collect_fixture(self, *, guide_node=None, page_mutation=None, max_bytes=16384,
                        cue="", seed_tags=(), clock=None):
        calls = []
        owner = self
        class Client:
            def __init__(self, *args, **kwargs):
                self.timeout = kwargs["timeout"]
            def connect(self):
                pass
            def close(self):
                pass
            def call_tool(self, name, args):
                calls.append((name, args))
                owner.assertEqual(args["db"], "project")
                owner.assertEqual(args["expected_db_id"], DB)
                if name == "get":
                    return guide_node
                prefix = args.get("prefix")
                names = ([prefix] if prefix in ("people", "rust", "numeric") or prefix in seed_tags
                         else ["aardvark"] if prefix is None else [])
                page = {"db": "project", "db_id": DB, "kind": "tags", **vocabulary(names),
                        "has_more": prefix is None, "next_cursor": "opaque" if prefix is None else None}
                if page_mutation:
                    page_mutation(page)
                return page
        config = {"tag_stewardship": True, "service_config": "/fixture/service.json",
                  "project_root": "/fixture/project"}
        if guide_node is not None:
            config["tag_guide_id"] = GUIDE
        with patch("mcp_client.McpClient", Client), patch("target_policy.target_kwargs", return_value={}), \
                patch("target_policy.policy_for", return_value=(SimpleNamespace(db_alias="project"),
                      SimpleNamespace(url="http://127.0.0.1:1", token_env=None))):
            if clock:
                with patch("tag_context.time.monotonic", side_effect=clock):
                    result = tags.collect(config, expected_db_id=DB, timeout=2, max_bytes=max_bytes,
                                          cue=cue, seed_tags=seed_tags)
            else:
                result = tags.collect(config, expected_db_id=DB, timeout=2, max_bytes=max_bytes,
                                      cue=cue, seed_tags=seed_tags)
        return result, calls

    def test_lookup_finds_relevant_names_beyond_first_alphabetical_page(self):
        result, calls = self.collect_fixture(cue="people rust numeric")
        self.assertTrue(result.enabled)
        self.assertEqual([item["name"] for item in result.vocabulary["items"]],
                         ["people", "rust", "numeric", "aardvark"])
        self.assertTrue(result.vocabulary["partial"])
        self.assertEqual([args.get("prefix") for _, args in calls], ["people", "rust", "numeric", None])
        self.assertLessEqual(result.decoded_bytes, 16384)

    def test_existing_tags_prioritized_and_work_follows_resource_envelope(self):
        result, calls = self.collect_fixture(cue="alpha beta gamma delta epsilon people rust numeric",
                                            seed_tags=["rust"], max_bytes=4096)
        self.assertTrue(result.enabled)
        self.assertEqual([args.get("prefix") for _, args in calls], ["rust", None])
        self.assertGreater(result.vocabulary["coverage"]["lookup_prefixes_omitted"], 0)
        self.assertTrue(result.vocabulary["partial"])
        self.assertLessEqual(result.decoded_bytes, 4096)

    def test_long_native_identity_and_large_seed_set_use_bounded_lookup_not_context_refusal(self):
        rare = "rare-" + "é" * 90
        seeds = [rare] + [f"identity-{i}" for i in range(63)]
        result, calls = self.collect_fixture(seed_tags=seeds)
        self.assertTrue(result.enabled)
        self.assertEqual(result.vocabulary["items"][0]["name"], rare)
        self.assertLess(len(calls), len(seeds))
        self.assertGreater(result.vocabulary["coverage"]["lookup_prefixes_omitted"], 0)
        self.assertTrue(result.vocabulary["partial"])
        self.assertLessEqual(result.decoded_bytes, tags.MAX_CONTEXT_BYTES)
        # A bounded native batch can have more distinct tags than one node.
        result, _ = self.collect_fixture(seed_tags=[f"identity-{i}-" + "x" * 190 for i in range(100)])
        self.assertTrue(result.enabled)
        self.assertGreater(result.vocabulary["coverage"]["seed_tags_omitted"], 0)
        self.assertTrue(result.vocabulary["partial"])

    def test_guide_complete_canonical_summary_archived_allowed_body_is_not_policy(self):
        text = guide()["text"]
        node = {"db": "project", "db_id": DB, "id": GUIDE, "memory_kind": {"kind": "semantic"}, "status": "archived",
                "body_ownership": "borrowed", "content_fingerprint": "a" * 64,
                "content_fingerprint_codec": "mneme.routing-content.v2",
                "summary": text, "summary_truncated": False}
        result, calls = self.collect_fixture(guide_node=node)
        self.assertTrue(result.enabled)
        self.assertNotIn("body", calls[0][1])
        self.assertEqual(result.guide, guide())
        for change in ({"db_id": OTHER}, {"memory_kind": {"kind": "episode"}},
                       {"content_fingerprint_codec": "old"}, {"summary": "\0"},
                       {"summary_truncated": True}, {"summary": "x" * 2049}):
            result, _ = self.collect_fixture(guide_node={**node, **change})
            self.assertFalse(result.enabled)

    def test_bad_owner_or_vocabulary_never_falls_back_and_budget_counts_failed_read(self):
        result, calls = self.collect_fixture(page_mutation=lambda page: page.update(db_id=OTHER))
        self.assertFalse(result.enabled)
        self.assertEqual(len(calls), 1)
        self.assertGreater(result.decoded_bytes, 0)
        result, calls = self.collect_fixture(max_bytes=512,
                                            page_mutation=lambda page: page.update(extra="x" * 1024))
        self.assertFalse(result.enabled)
        self.assertGreater(result.decoded_bytes, 512)

    def test_disabled_does_no_work_and_deadline_stops_lookups(self):
        with patch("mcp_client.McpClient", side_effect=AssertionError("must not connect")):
            self.assertFalse(tags.collect({}, expected_db_id=DB, timeout=2).enabled)
        result, calls = self.collect_fixture(cue="people rust", clock=[0, 1, 1.5, 3])
        self.assertTrue(result.enabled)
        self.assertEqual(len(calls), 1)
        self.assertTrue(result.vocabulary["partial"])


if __name__ == "__main__":
    unittest.main()
