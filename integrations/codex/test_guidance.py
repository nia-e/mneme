"""Provider-free canonical tag guides and existing project instruction files."""
import copy
import hashlib
from pathlib import Path
import sys
import tempfile
import time
from types import SimpleNamespace
import unittest
from unittest.mock import patch

sys.path.insert(0, str(Path(__file__).parent))
import guidance as g
import recording_jobs as jobs
import tag_context

DB = "00000000000000000000000001"
NODE = "00000000000000000000000002"
OTHER_DB = "00000000000000000000000003"
BINDING = {"selection_digest": "c"*64, "name": "owner"}
SELECTED = {"tag_guide": {"id": NODE, "content": "summary"}}


def node(**changes):
    return {"db": "owner", "db_id": DB, "id": NODE,
            "memory_kind": {"kind": "semantic"}, "status": "archived",
            "summary": "Preserve exact editions, never flatten identity.", "summary_truncated": False,
            "content_fingerprint": "a"*64, "content_fingerprint_codec": g.CODEC,
            "provenance": {"type": "external", "source": {"revision": "original-sha"}}, **changes}


class Owner:
    def __init__(self, value=None, *, db_id=DB, room=65536):
        self.value = node() if value is None else value
        self.db_id = db_id
        self.policy = SimpleNamespace(db_alias="owner")
        self.remaining_bytes = room
        self.deadline = time.monotonic()+30
        self.calls = []
    def call(self, name, args):
        self.calls.append((name, args))
        result = copy.deepcopy(self.value)
        self.remaining_bytes -= len(g.encoded(result))
        if self.remaining_bytes < 0:
            raise ValueError("native_byte_budget")
        return result


class GuidanceTests(unittest.TestCase):
    def collect(self, owner=None, selected=None):
        return g.collect_guidance(owner or Owner(), SELECTED if selected is None else selected,
                                  owner_binding=BINDING)

    def test_static_roles_keep_existing_tag_guide_only(self):
        for value in ([1], {"unknown": {}}, {"tag_guide": {"id": NODE, "content": "body"}},
                      {"hippocampus": {"id": NODE, "content": "summary"}},
                      {"tag_guide": {"id": "bad", "content": "summary"}},
                      {"tag_guide": {"id": NODE, "content": "summary", "text": "policy"}}):
            with self.subTest(value=value), self.assertRaises(ValueError): g.selectors(value)
        owner = Owner()
        self.assertEqual(self.collect(owner, {}).roles, {"tag_guide": {"outcome": "not_configured"}})
        self.assertEqual(owner.calls, [])

    def test_complete_summary_witness_is_immutable_and_never_reads_body(self):
        owner = Owner(node(body="not tag policy", body_revision="not used"))
        snap = self.collect(owner)
        self.assertEqual(owner.calls, [("get", {"db": "owner", "expected_db_id": DB, "id": NODE})])
        witness = snap.roles["tag_guide"]["witness"]
        self.assertEqual(witness["text"], node()["summary"])
        self.assertNotIn("body_revision", witness)
        self.assertEqual(snap.decoded_bytes, 65536-owner.remaining_bytes)
        self.assertEqual(snap.content_guards, [{"id": NODE, "content_fingerprint": "a"*64}])
        witness["text"] = "not a shared mutation"
        self.assertEqual(snap.roles["tag_guide"]["witness"]["text"], node()["summary"])
        g.validate_snapshot(snap)

    def test_native_id_collisions_remain_owner_qualified(self):
        first = self.collect()
        second = self.collect(Owner(node(db_id=OTHER_DB, summary="another owner"), db_id=OTHER_DB))
        self.assertEqual(second.roles["tag_guide"]["outcome"], "ok")
        self.assertNotEqual(first.to_dict()["db_id"], second.to_dict()["db_id"])
        with self.assertRaisesRegex(ValueError, "guidance_stale"):
            g.revalidate_guidance(first, Owner(db_id=OTHER_DB), SELECTED, owner_binding=BINDING)
        for mutation in ({"db": "other"}, {"db_id": OTHER_DB}, {"id": OTHER_DB},
                         {"memory_kind": {"kind": "episode"}}, {"content_fingerprint_codec": "old"}):
            self.assertEqual(self.collect(Owner(node(**mutation))).roles["tag_guide"]["outcome"], "invalid")

    def test_summary_not_a_card_no_silent_prefix_policy(self):
        self.assertEqual(self.collect(Owner(node(summary_truncated=True))).roles["tag_guide"]["outcome"], "incomplete")
        for text in ("x"*2049, "\0", "   "):
            self.assertEqual(self.collect(Owner(node(summary=text))).roles["tag_guide"]["outcome"], "invalid")
        text = "# canonical guide\n\né and authored nuance\n"
        self.assertEqual(self.collect(Owner(node(summary=text))).roles["tag_guide"]["witness"]["text"], text)

    def test_all_consulted_revisions_and_binding_rechecked(self):
        snap = self.collect()
        g.revalidate_guidance(snap, Owner(), SELECTED, owner_binding=BINDING)
        for changed in ({"summary": "new summary"}, {"provenance": {"revision": "new-source"}},
                        {"content_fingerprint": "b"*64}):
            with self.subTest(changed=changed), self.assertRaisesRegex(ValueError, "guidance_stale"):
                g.revalidate_guidance(snap, Owner(node(**changed)), SELECTED, owner_binding=BINDING)
        with self.assertRaisesRegex(ValueError, "guidance_stale"):
            g.revalidate_guidance(snap, Owner(), SELECTED, owner_binding={"changed": True})
        with self.assertRaisesRegex(ValueError, "guidance_stale"):
            g.revalidate_guidance(snap, Owner(), {"tag_guide": {"id": OTHER_DB, "content": "summary"}}, owner_binding=BINDING)

    def test_missing_unavailable_and_budget_are_not_success_shaped_empty_guides(self):
        self.assertEqual(self.collect(Owner({"found": False})).roles["tag_guide"]["outcome"], "missing")
        owner = Owner(room=0)
        self.assertEqual(self.collect(owner).roles["tag_guide"]["outcome"], "budget_omitted")
        self.assertEqual(owner.calls, [])
        snap = self.collect(Owner(room=20))
        self.assertEqual(snap.roles["tag_guide"]["outcome"], "budget_omitted")
        self.assertGreater(snap.decoded_bytes, 20)
        owner = Owner()
        def fail(*args): raise OSError("offline")
        owner.call = fail
        self.assertEqual(self.collect(owner).roles["tag_guide"]["outcome"], "unavailable")
        self.assertEqual(owner.remaining_bytes, 0)

    def test_cached_edition_cannot_certify_live_guide(self):
        snapshot = self.collect(Owner(node(_mneme_source={"generation": "old", "owner_db_id": DB})))
        self.assertEqual(snapshot.roles["tag_guide"]["outcome"], "unavailable")
        self.assertGreater(snapshot.decoded_bytes, 0)

    def test_failed_native_read_charges_framing_and_passes_remaining_budget(self):
        from mcp_client import McpToolError
        class Client:
            work_bytes = 0
            def connect(self): pass
            def close(self): pass
            def call_tool(self, name, args):
                self.work_bytes += 513
                raise McpToolError("node not found")
        client = Client()
        policy = SimpleNamespace(native=True, db_alias="owner", validate_workspace=lambda: None)
        service = SimpleNamespace(token_env=None, native_client=lambda **kwargs: client)
        with g.GuidanceOwner(policy, service, DB, time.monotonic()+3, 600) as owner:
            snap = self.collect(owner)
            self.assertEqual(owner.remaining_bytes, 87)
            self.assertEqual(snap.decoded_bytes, 513)
            self.assertEqual(client.max_native_bytes, 600)
            self.assertEqual(snap.roles["tag_guide"]["outcome"], "missing")

    def test_changed_hash_not_independently_accepted_and_receipt_is_qualified(self):
        snapshot = self.collect().to_dict()
        receipt = jobs.guidance_receipt(snapshot)
        self.assertEqual(receipt["db_id"], DB)
        self.assertEqual(receipt["save_revalidation"], "host_non_atomic")
        self.assertNotIn("text", receipt["roles"]["tag_guide"]["witness"])
        snapshot["roles"]["tag_guide"]["witness"]["text"] = "different meaning"
        with self.assertRaises(ValueError): g.validate_snapshot(snapshot)


class ExistingProjectFocusTests(unittest.TestCase):
    def setUp(self):
        self.tmp = tempfile.TemporaryDirectory()
        self.addCleanup(self.tmp.cleanup)
        self.root = Path(self.tmp.name)
        self.path = self.root / ".mneme" / "hippocampus.md"
        self.path.parent.mkdir()
        self.store = SimpleNamespace(workspace_root=str(self.root))

    def test_exact_existing_markdown_is_file_instructions_not_node_content(self):
        text = "# hippocampus\n\nKeep my deliberately authored weirdness, é included.\n"
        self.path.write_text(text)
        loaded, witness = jobs.project_focus_witness(self.root)
        self.assertEqual(loaded, text)
        jobs.revalidate_project_focus(self.store, {"project_focus_witness": witness})
        self.assertEqual(witness["root"], str(self.root))
        self.assertEqual(set(witness), {"root", "sha256"})
        self.path.write_text(text + "Different instructions.\n")
        with self.assertRaisesRegex(ValueError, "policy changed"):
            jobs.revalidate_project_focus(self.store, {"project_focus_witness": witness})

    def test_absence_is_frozen_and_new_file_refuses_old_assessment(self):
        text, witness = jobs.project_focus_witness(self.root)
        self.assertIsNone(text)
        jobs.revalidate_project_focus(self.store, {"project_focus_witness": witness})
        self.path.write_text("New instructions.")
        with self.assertRaises(ValueError): jobs.revalidate_project_focus(self.store, {"project_focus_witness": witness})

    def test_current_byte_limit_no_follow_and_exact_utf8_are_preserved(self):
        for raw in (b"x"*2049, b"\xff", b"bad\0instructions"):
            self.path.write_bytes(raw)
            with self.assertRaises(ValueError): jobs.project_focus_witness(self.root)
        self.path.unlink()
        target = self.root / "elsewhere.md"
        target.write_text("Do not follow this replacement file.")
        self.path.symlink_to(target)
        with self.assertRaises(ValueError): jobs.project_focus_witness(self.root)

    def test_nonproject_scopes_never_read_a_file_or_inherit_project_instructions(self):
        with patch("recording_jobs.project_focus_witness", side_effect=AssertionError("not a project")):
            jobs.revalidate_project_focus(SimpleNamespace(workspace_root=None), {})
            with self.assertRaises(ValueError):
                jobs.revalidate_project_focus(SimpleNamespace(workspace_root=None),
                    {"project_focus_witness": {"root": str(self.root), "sha256": "a"*64}})
        with self.assertRaises(ValueError): jobs.project_focus_witness("relative-root")

    def test_wrong_project_root_binding_refuses_without_reading_other_project(self):
        _, witness = jobs.project_focus_witness(self.root)
        with patch("recording_jobs.project_focus_witness", side_effect=AssertionError("must not read")):
            with self.assertRaises(ValueError):
                jobs.revalidate_project_focus(SimpleNamespace(workspace_root="/another/project"),
                    {"project_focus_witness": witness})


if __name__ == "__main__": unittest.main()
