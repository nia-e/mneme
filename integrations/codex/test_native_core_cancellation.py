"""Finite native collector cancellation and pre-execution identity fences."""
import io
import json
from pathlib import Path
import sys
import tempfile
import time
from types import SimpleNamespace
import unittest
from unittest.mock import patch

import core_hook
import store_registry as registry


class NativeCoreCancellationTests(unittest.TestCase):
    def setUp(self):
        self.temp = tempfile.TemporaryDirectory()
        self.addCleanup(self.temp.cleanup)
        self.root = Path(self.temp.name).resolve()
        self.binary = self.root / "mnemed"
        self.binary.write_bytes(b"original native identity")
        self.value = {"schema": "mneme.config-selection.v1", "digest": "a" * 64,
                      "active": [], "config": {}, "cwd": str(self.root)}

    def selected(self, *, pin=None):
        return registry.NativeSelection(self.value, str(self.root / "memory.toml"),
                                        str(self.binary), str(self.root),
                                        registry.native_binary_identity(self.binary) if pin is None else pin)

    def witness(self, selection):
        return {"schema": "mneme.native-core-selection.v1", "config_path": selection.registry_path,
                "native_cli": selection.native_cli, "cwd": selection.cwd,
                "digest": selection.digest, "binary_pin": selection.binary_pin}

    @unittest.skipIf(sys.platform == "win32", "POSIX executable/process-group fixture")
    def test_collector_timeout_reaps_native_selector_before_it_can_finish(self):
        started, finished = self.root / "selector-started", self.root / "selector-finished"
        self.binary.write_text(
            "#!" + sys.executable + "\nimport time\nfrom pathlib import Path\n"
            + "Path(" + repr(str(started)) + ").write_text('started')\n"
            + "time.sleep(.9)\n"
            + "Path(" + repr(str(finished)) + ").write_text('finished')\n"
            + "print(" + repr(json.dumps(self.value)) + ")\n")
        self.binary.chmod(0o700)
        self.assertEqual(core_hook.collect_registry_core(self.selected(), timeout=.6), [])
        self.assertTrue(started.exists(), "fixture must reach native execution before cancellation")
        self.assertFalse(finished.exists())
        # The deliberately finite child would finish by now if the collector
        # killed only its Python child and left the native process behind.
        time.sleep(.95)
        self.assertFalse(finished.exists(), "native selector survived collector cancellation")

    def test_unchanged_explicit_legacy_core_pin_survives_unpinned_native_replay(self):
        selected = self.selected()
        replay = registry.NativeSelection(self.value, selected.registry_path,
                                          selected.native_cli, selected.cwd)
        request = {"selection": self.witness(selected), "timeout": 1}
        with patch("store_registry.native_select", return_value=replay) as select:
            self.assertEqual(core_hook._registry_child_collect(request, None, time.monotonic() + 1), 0)
        select.assert_called_once()

    def test_changed_binary_refuses_frozen_revalidation_before_native_execution(self):
        selected = self.selected()
        self.binary.write_bytes(b"replacement executable")
        for route in ("revalidate", "from_dict"):
            with self.subTest(route=route), patch("store_registry.native_select") as select:
                with self.assertRaises(ValueError):
                    if route == "revalidate":
                        selected.revalidate()
                    else:
                        registry.NativeSelection.from_dict(selected.to_dict())
                select.assert_not_called()

    def test_changed_or_malformed_compact_pin_refuses_before_native_execution(self):
        selected = self.selected()
        for mode in ("changed", "malformed"):
            witness = self.witness(selected)
            if mode == "changed":
                self.binary.write_bytes(b"replacement executable")
            else:
                witness["binary_pin"] = "not-an-identity"
            request = json.dumps({"selection": witness, "timeout": 1}).encode()
            with self.subTest(mode=mode), \
                 patch("core_hook.sys.stdin", SimpleNamespace(buffer=io.BytesIO(request))), \
                 patch("store_registry.native_select") as select, \
                 patch("core_hook._collect_registry_scope") as read:
                self.assertEqual(core_hook._registry_child_main(), 1)
                select.assert_not_called()
                read.assert_not_called()

    def test_malformed_snapshot_binary_pin_is_rejected_before_revalidation(self):
        for pin in (True, False, [], {}, "not-an-identity", {"device": 1}):
            with self.subTest(pin=pin), patch("store_registry.native_select") as select:
                with self.assertRaises(ValueError):
                    self.selected(pin=pin)
                select.assert_not_called()


if __name__ == "__main__":
    unittest.main()
