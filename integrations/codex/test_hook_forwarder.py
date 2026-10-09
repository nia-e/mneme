"""Disposable copied-entry fixtures; no owner, provider or installed runtime."""
import copy
import hashlib
import json
import os
from pathlib import Path
import subprocess
import sys
import tempfile
import unittest

import hook_forwarder


FIXTURE = b'''import json, os, sys
raw = sys.stdin.buffer.read()
print(json.dumps({"argv": sys.argv, "stdin_hex": raw.hex(), "cwd": os.getcwd(),
                  "environment": os.environ.get("MNEME_FORWARDER_FIXTURE")}))
print("target stderr", file=sys.stderr)
raise SystemExit(int(os.environ.get("MNEME_FORWARDER_EXIT", "0")))
'''


def sha(raw):
    return hashlib.sha256(raw).hexdigest()


class HookForwarderTests(unittest.TestCase):
    def setUp(self):
        temporary = tempfile.TemporaryDirectory()
        self.addCleanup(temporary.cleanup)
        self.base = Path(temporary.name).resolve()
        # Spaces and Unicode must survive Python literals and argv without shells.
        self.source = self.base / "retired café" / "hooks.py"
        self.source.parent.mkdir()
        self.target = self.base / "qualified runtime" / "hooks.py"
        self.target.parent.mkdir()
        self.target.write_bytes(FIXTURE)
        self.old_config = self.source.parent / "old config.json"
        self.old_config.write_bytes(b'{"owner": "fixture", "old": true}\n')
        self.new_config = self.target.parent / "new config.json"
        self.new_config.write_bytes(b'{"owner": "fixture", "old": false}\n')
        self.cwd = self.base / "working directory"
        self.cwd.mkdir()
        self.routes = {str(self.old_config): {
            "old_config_sha256": sha(self.old_config.read_bytes()),
            "target_script": str(self.target),
            "target_script_sha256": sha(FIXTURE),
            "target_config": str(self.new_config),
            "target_config_sha256": sha(self.new_config.read_bytes()),
        }}
        self.publish()

    def publish(self):
        self.source.write_bytes(hook_forwarder.render_forwarder(str(self.source), self.routes))

    def invoke(self, args, payload=b"", *, entry=None, exit_code=0):
        return subprocess.run(
            [sys.executable, "-B", str(entry or self.source), *args], input=payload,
            cwd=self.cwd, env={**os.environ, "MNEME_FORWARDER_FIXTURE": "unchanged value",
                               "MNEME_FORWARDER_EXIT": str(exit_code)},
            capture_output=True, timeout=5,
        )

    def assert_refused(self, result):
        self.assertEqual(result.returncode, 1)
        self.assertEqual(result.stdout, b"")
        self.assertEqual(result.stderr, (hook_forwarder.NOTICE + "\n").encode())

    def test_render_is_deterministic_exact_and_does_not_publish(self):
        destination = str(self.base / "not-created.py")
        route = self.routes[str(self.old_config)]
        rendered = hook_forwarder.render_forwarder(destination, self.routes)
        reordered = {str(self.old_config): dict(reversed(list(route.items())))}
        self.assertEqual(rendered, hook_forwarder.render_forwarder(destination, reordered))
        self.assertFalse(Path(destination).exists())
        hook_forwarder.validate_forwarder(rendered, destination, self.routes)
        for changed in (rendered + b"\n", rendered.decode(), b"unreviewed script"):
            with self.subTest(changed_type=type(changed).__name__), self.assertRaises(ValueError):
                hook_forwarder.validate_forwarder(changed, destination, self.routes)

    def test_spec_rejects_ambiguous_paths_fields_pins_and_self_target(self):
        source = str(self.source)
        invalid = []
        for path in ("relative.py", "//host/path.py", source + "/../hooks.py",
                     "/tmp/bad\npath.py", "/" + "x" * 4096):
            invalid.append((path, self.routes))
        invalid.extend(((source, {}), (source, [])))
        for field, value in (("old_config_sha256", "A" * 64),
                             ("target_script_sha256", "f" * 63),
                             ("target_config_sha256", 123),
                             ("target_script", source),
                             ("target_config", source),
                             ("target_config", "relative.json")):
            routes = copy.deepcopy(self.routes)
            routes[str(self.old_config)][field] = value
            invalid.append((source, routes))
        routes = copy.deepcopy(self.routes)
        routes[str(self.old_config)]["extra"] = True
        invalid.append((source, routes))
        routes = copy.deepcopy(self.routes)
        del routes[str(self.old_config)]["old_config_sha256"]
        invalid.append((source, routes))
        for candidate, routes in invalid:
            with self.subTest(source=candidate, routes=routes), self.assertRaises(ValueError):
                hook_forwarder.render_forwarder(candidate, routes)

    def test_event_bytes_arguments_environment_and_cwd_survive(self):
        payload = b'\x00\xffnot inspected JSON\n' + b"body" * 100_000
        args = ["--config", str(self.old_config), "--reader-background",
                "--workspace-binding", '{"workspace_root":"space ; $(no shell)"}']
        result = self.invoke(args, payload, exit_code=23)
        self.assertEqual(result.returncode, 23)
        self.assertEqual(result.stderr, b"target stderr\n")
        forwarded = json.loads(result.stdout)
        self.assertEqual(forwarded, {
            "argv": [str(self.target), "--config", str(self.new_config), *args[2:]],
            "stdin_hex": payload.hex(), "cwd": str(self.cwd),
            "environment": "unchanged value",
        })

    def test_checkpoint_config_position_and_equals_spelling_are_preserved(self):
        checkpoint = ["checkpoint", "--session-id", "existing:session", "--turn-id", "turn.2",
                      "--outcome", "captured", "--id", "01ARZ3NDEKTSV4RRFFQ69G5FAV"]
        for before, after in ((["--config", str(self.old_config), *checkpoint],
                               ["--config", str(self.new_config), *checkpoint]),
                              ([*checkpoint, "--config", str(self.old_config)],
                               [*checkpoint, "--config", str(self.new_config)]),
                              (["--config=" + str(self.old_config), *checkpoint],
                               ["--config=" + str(self.new_config), *checkpoint])):
            with self.subTest(args=before):
                result = self.invoke(before)
                self.assertEqual(result.returncode, 0)
                self.assertEqual(json.loads(result.stdout)["argv"], [str(self.target), *after])

    def test_multiple_explicit_routes_include_misc_launcher_without_dispatch_bypass(self):
        misc_config = self.base / "misc-old.json"
        misc_config.write_bytes(b'{"memory_scope":"misc"}')
        misc_new = self.target.parent / "misc-new.json"
        misc_new.write_bytes(misc_config.read_bytes())
        launcher = self.target.parent / "hook_launcher.py"
        launcher.write_bytes(FIXTURE)
        self.routes[str(misc_config)] = {
            "old_config_sha256": sha(misc_config.read_bytes()),
            "target_script": str(launcher), "target_script_sha256": sha(FIXTURE),
            "target_config": str(misc_new), "target_config_sha256": sha(misc_new.read_bytes()),
        }
        self.publish()
        for old, target, new in ((self.old_config, self.target, self.new_config),
                                 (misc_config, launcher, misc_new)):
            with self.subTest(config=old):
                result = self.invoke(["--config", str(old)])
                self.assertEqual(result.returncode, 0)
                self.assertEqual(json.loads(result.stdout)["argv"], [str(target), "--config", str(new)])

    def test_missing_unknown_duplicate_and_abbreviated_config_are_refused(self):
        known = ["--config", str(self.old_config)]
        invalid = [[], ["--config"], ["--config", str(self.base / "unknown.json")],
                   [*known, *known], [*known, "--config=" + str(self.old_config)],
                   ["--config=" + str(self.old_config), *known]]
        for option in ("--c", "--co", "--con", "--conf", "--confi", "--config-extra"):
            invalid.extend(([option, str(self.old_config)],
                            [*known, option, str(self.old_config)],
                            [*known, option + "=" + str(self.old_config)]))
        for args in invalid:
            with self.subTest(args=args):
                self.assert_refused(self.invoke(args, b"private event"))

    def test_changed_or_missing_pinned_files_fail_without_target_output(self):
        for path in (self.old_config, self.new_config, self.target):
            original = path.read_bytes()
            try:
                with self.subTest(path=path, change="bytes"):
                    path.write_bytes(original + b"\n")
                    self.assert_refused(self.invoke(["--config", str(self.old_config)]))
                with self.subTest(path=path, change="missing"):
                    path.unlink()
                    self.assert_refused(self.invoke(["--config", str(self.old_config)]))
            finally:
                path.write_bytes(original)

    def test_nonregular_linked_and_oversized_pinned_files_fail(self):
        for path in (self.old_config, self.new_config, self.target):
            original = path.read_bytes()
            backing = path.with_suffix(".backing")
            backing.write_bytes(original)
            for kind in ("symlink", "hardlink", "directory", "fifo", "oversized"):
                with self.subTest(path=path, kind=kind):
                    path.unlink()
                    if kind == "symlink":
                        path.symlink_to(backing)
                    elif kind == "hardlink":
                        os.link(backing, path)
                    elif kind == "directory":
                        path.mkdir()
                    elif kind == "fifo":
                        os.mkfifo(path)
                    else:
                        path.write_bytes(b"x" * (1024 * 1024 + 1 if path == self.target else 8001))
                    try:
                        self.assert_refused(self.invoke(["--config", str(self.old_config)]))
                    finally:
                        if kind == "directory":
                            path.rmdir()
                        else:
                            path.unlink()
                        path.write_bytes(original)

    def test_forwarder_chain_is_refused_even_when_target_hash_matches(self):
        chained = ("#!/usr/bin/env python3\n" + hook_forwarder.MARKER + "\n").encode() + FIXTURE
        self.target.write_bytes(chained)
        self.routes[str(self.old_config)]["target_script_sha256"] = sha(chained)
        self.publish()
        self.assert_refused(self.invoke(["--config", str(self.old_config)]))

    def test_relocated_symlinked_or_hardlinked_entry_is_refused(self):
        duplicate = self.base / "relocated.py"
        duplicate.write_bytes(self.source.read_bytes())
        self.assert_refused(self.invoke(["--config", str(self.old_config)], entry=duplicate))
        duplicate.unlink()
        duplicate.symlink_to(self.source)
        self.assert_refused(self.invoke(["--config", str(self.old_config)], entry=duplicate))
        duplicate.unlink()
        os.link(self.source, duplicate)
        self.assert_refused(self.invoke(["--config", str(self.old_config)]))


if __name__ == "__main__":
    unittest.main()
