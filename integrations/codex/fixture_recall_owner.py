"""Disposable read-owner and overlap fixtures; no test-suite lifecycle borrowing."""
import json
from pathlib import Path
import tempfile
from unittest.mock import patch

import hook_recall


ID1 = "01ARZ3NDEKTSV4RRFFQ69G5FAV"
ID2 = "01ARZ3NDEKTSV4RRFFQ69G5FAW"
ID3 = "01ARZ3NDEKTSV4RRFFQ69G5FAX"
ID4 = "01ARZ3NDEKTSV4RRFFQ69G5FAY"
ID5 = "01ARZ3NDEKTSV4RRFFQ69G5FAZ"
ID6 = "01ARZ3NDEKTSV4RRFFQ69G5FB0"
ID7 = "01ARZ3NDEKTSV4RRFFQ69G5FB1"


def reference_origin(target, *, episode_anchor=False, anchor_id=None):
    anchor_id = anchor_id or (ID5 if episode_anchor else ID1)
    anchor = ({"kind": "episode", "identity": {"episode_id": ID6,
               "edition_id": anchor_id, "revision": 2}} if episode_anchor else
              {"kind": "semantic", "node_id": anchor_id})
    return {"kind": "reference", "anchor": anchor, "from": anchor_id,
            "to": target, "edge_kind": "Associative", "body_anchor": None}


def discovery_metadata():
    return {"partial": False,
            "omitted": {lane: {"bounded_window_budget": 0, "further_tail_unknown": False}
                        for lane in ("core", "primary", "expansion", "episodic")},
            "retrieval": {"mode": "untagged", "partial": False, "work": None,
                          "lanes": {"primary": {"seed_coverage": None}},
                          "stamp": {"secret": "must never enter diagnostics"}},
            "episodic_retrieval": {"state": "searched", "mode": "lexical",
                                   "cue_normalized": True, "cue_truncated": False},
            "episode_reference_retrieval": {"state": "searched",
                **dict.fromkeys(hook_recall._REFERENCE_COUNTS, 0), "further_tail_unknown": False}}


class FakeClient:
    def __init__(self, catalog, context, nodes):
        self.catalog, self.context, self.nodes = catalog, context, nodes
        self.calls = []

    def __enter__(self):
        return self

    def __exit__(self, *_):
        return False

    def call_tool(self, name, args):
        self.calls.append((name, args))
        if name == "databases":
            return self.catalog
        if name == "recall_context":
            return self.context
        if name == "get":
            return self.nodes[args["id"]]
        raise AssertionError("unexpected or mutating tool: " + name)


class RecallOwnerFixture:
    def __init__(self, checks):
        self.temp = tempfile.TemporaryDirectory()
        checks.addCleanup(self.temp.cleanup)
        self.root = Path(self.temp.name)
        store = self.root / ".mneme" / "codex-memory.db"
        store.parent.mkdir()
        store.write_bytes(b"test fixture; not opened")
        self.store = store
        self.config = self.root / "service.json"
        self.config.write_text(json.dumps({
            "binary": str(self.root / "not-used"), "project_db": str(store),
            "working_directory": str(self.root), "state_dir": str(self.root / "private"),
            "port": 18765,
        }))
        self.catalog = [{"db": "project", "name": "project", "state": "open",
                         "configured_path": str(store)}]
        self.context = {"schema": "mneme.context.v6", "core": [],
                        "primary": [{"id": ID1}, {"id": ID1}, {"id": ID2}],
                        "expansions": [{"id": ID3}], "episodes": []}
        self.nodes = {
            ID1: self._node(ID1, "active", "Evidence one"),
            ID2: self._node(ID2, "active", "Evidence two"),
            ID3: self._node(ID3, "active", "Evidence three"),
        }

    def _node(self, identifier, status, summary):
        return {"id": identifier, "status": status, "summary": summary,
                "summary_truncated": False,
                "provenance": {"type": "external", "source": {
                    "namespace": "codex", "key": identifier,
                    "reference": "codex://test/%s" % identifier}}}

    def _connect_config(self, **changes):
        data = {"mode": "connect", "url": "http://127.0.0.1:18765/",
                "database_name": "project",
                "database_path": str(self.root / "remote-owner" / "project-memory.db")}
        data.update(changes)
        self.config.write_text(json.dumps(data))
        return data


class OverlapFixture(RecallOwnerFixture):
    def __init__(self, checks):
        super().__init__(checks)
        self.db_id = ID7
        self.catalog[0]["db_id"] = self.db_id

    def overlap(self, client, **kwargs):
        with patch("hook_recall.McpClient", return_value=client):
            return hook_recall.collect_overlap(self.config, "bounded task cue", self.root,
                                               expected_db_id=self.db_id, **kwargs)
