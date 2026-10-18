#!/usr/bin/env python3
"""Validate actual Home Manager symlinks with the packaged native CLI, offline."""

from __future__ import annotations

import hashlib
import json
import os
from pathlib import Path
import subprocess
import sys
import tempfile
import tomllib


def require(condition, message):
    if not condition:
        raise RuntimeError(message)


def select(cli, config, root, env, *, success):
    result = subprocess.run(
        [str(cli), "--json", "config", "select", "--config", str(config), "--cwd", str(root)],
        env=env, cwd=root, stdin=subprocess.DEVNULL, stdout=subprocess.PIPE,
        stderr=subprocess.PIPE, text=True, timeout=20,
    )
    require((result.returncode == 0) == success,
            f"unexpected selection exit {result.returncode}: {result.stdout}\n{result.stderr}")
    if success:
        selected = json.loads(result.stdout)
        require(selected["read_default"] == "fixture" and selected["digest"],
                f"wrong native selection: {selected!r}")
    else:
        require("invalid Mneme TOML" in result.stdout + result.stderr,
                f"failure was not native schema validation: {result.stdout}\n{result.stderr}")


def main():
    manifest = json.loads(Path(sys.argv[1]).read_text())
    prefix = Path(manifest["package"])
    cli = prefix / "bin/mnemed"
    metadata = json.loads((prefix / "share/mneme/build-info.json").read_text())
    require(metadata["defaultFeatures"] is False and metadata["features"] == [],
            "expected the packaged no-default-features client")
    for line in (prefix / "share/mneme/binary-sha256.txt").read_text().splitlines():
        expected, name = line.split()
        with (prefix / name).open("rb") as binary:
            require(hashlib.file_digest(binary, "sha256").hexdigest() == expected,
                    f"installed binary hash mismatch: {name}")
    with tempfile.TemporaryDirectory(prefix="mneme-hm-test-") as scratch:
        root = Path(scratch).resolve()
        home, cwd, stub = [root / part for part in ("home", "cwd", "bin")]
        for path in (home, cwd, stub):
            path.mkdir()
        marker = root / "ssh-contacted"
        ssh = stub / "ssh"
        ssh.write_text(f"#!{sys.executable}\nfrom pathlib import Path\n"
                       f"Path({str(marker)!r}).write_text('unexpected contact')\nraise SystemExit(97)\n")
        ssh.chmod(0o755)
        env = {"HOME": str(home), "PATH": str(stub), "XDG_CONFIG_HOME": str(home / ".config"),
               "XDG_DATA_HOME": str(home / ".local/share"), "XDG_CACHE_HOME": str(home / ".cache"),
               "TMPDIR": str(root), "LANG": "C"}
        for name, fixture in manifest["fixtures"].items():
            generated = Path(fixture["generation"]) / fixture["relative"]
            require(generated.is_symlink(), f"not an actual Home Manager generated symlink: {generated}")
            require(generated.resolve() == Path(fixture["source"]).resolve(),
                    "Home Manager generated link points to the wrong TOML source")
            parsed = tomllib.loads(generated.read_text())
            valid = name in {"valid", "merged", "customXdg"}
            if valid:
                require(parsed == manifest["settings"], f"TOML changed native values/defaults: {name}")
            elif name == "empty":
                require(parsed == {}, "empty settings received invented defaults")
            elif name == "unknown":
                require(parsed.get("misspelled_native_field") is True, "unknown native field was dropped")
            elif name == "wrongType":
                require(parsed["background"]["enabled"] == "false", "wrong native type was coerced")
            # Match Home Manager's link-to-generation topology without activating
            # any profile or running activation scripts in a real home directory.
            installed = home / fixture["relative"]
            installed.parent.mkdir(parents=True, exist_ok=True)
            installed.symlink_to(generated)
            select(cli, installed, cwd, env, success=valid)
            require(not marker.exists(), "metadata-only selection invoked SSH")
            require(not list(cwd.iterdir()), "configuration selection created workspace state")
            require(not (home / ".local").exists() and not (home / ".cache").exists(),
                    "configuration selection created persistent user state")
            installed.unlink()
            print(f"passed: generated Home Manager symlink and native validation: {name}")
    print(f"passed: {len(manifest['checks'])} Home Manager evaluation assertions; no activation or owner contact")


if __name__ == "__main__":
    main()
