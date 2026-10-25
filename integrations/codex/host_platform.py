"""Host naming and Codex hook command rendering, without routing policy.

Codex 0.162.1 runs hooks with the session shell: Windows normally PowerShell,
with cmd as a fallback. CRT argv quoting is not itself safe shell quoting. The
Windows envelope below puts user paths solely inside an encoded script, then
starts the actual executable directly with ProcessStartInfo and inherited I/O.
"""
from __future__ import annotations

import base64
import hashlib
import os
from pathlib import Path
import re
import shlex
import shutil
import subprocess
import sys

WINDOWS = os.name == "nt"
PYTHON_CANDIDATES = ("python3", "python3.14", "python3.13", "python3.12", "python3.11")
PYTHON_PROBE = "import sys; raise SystemExit(sys.version_info < (3, 11))"


def python_path_candidates():
    """Match init's name-first search, including shadowed compatible interpreters."""
    directories = os.environ.get("PATH", "").split(os.pathsep)
    for name in PYTHON_CANDIDATES:
        for directory in directories:
            yield os.path.join(directory or os.curdir, name)


def python_path_script(arguments=(), *, unavailable="Mneme requires Python 3.11 or newer on PATH"):
    """Render one finite PATH search; paths/argv stay data, never shell source.

    An incompatible first python3 must not hide a supported later python3 that
    init can discover. Split PATH with parameter expansion, not unquoted shell
    expansion: spaces, glob characters, and empty entries retain their meaning.
    The host's existing hook deadline bounds these launch-time version probes.
    """
    args = shlex.join(arguments)
    return ("for python in " + shlex.join(PYTHON_CANDIDATES) + "; do "
            "path_tail=${PATH-}; while :; do "
            'case "$path_tail" in *:*) directory=${path_tail%%:*}; path_tail=${path_tail#*:}; more=1;; '
            '*) directory=$path_tail; more=;; esac; '
            'candidate=${directory:-.}/$python; '
            'case "$candidate" in /*) ;; *) candidate=./$candidate;; esac; '
            'if [ -f "$candidate" ] && [ -x "$candidate" ] && "$candidate" -c '
            + shlex.quote(PYTHON_PROBE) + ' >/dev/null 2>&1; then exec "$candidate" '
            + (args + " " if args else "") + '"$@"; fi; '
            '[ -n "$more" ] || break; done; done; '
            "printf '%s\\n' " + shlex.quote(unavailable) + " >&2; exit 127")


def executable_name(name, *, windows=None):
    """Native executable basename; never accept a path or command script."""
    if not isinstance(name, str) or not name or any(c in name for c in "/\\\x00\r\n"):
        raise ValueError("expected executable basename")
    windows = WINDOWS if windows is None else windows
    return name if not windows or name.lower().endswith(".exe") else name + ".exe"


def is_executable(path, *, windows=None):
    """Native file executable, not a Windows .cmd/.bat shell trampoline."""
    windows = WINDOWS if windows is None else windows
    path = Path(path)
    return path.is_file() and (path.suffix.lower() == ".exe" if windows else os.access(path, os.X_OK))


def validate_command(value, *, default):
    """Validate a logical PATH command or lexical absolute executable selector.

    Selectors identify replaceable tooling, not an executable edition. Absolute
    symlinks deliberately retain their spelling across package-manager updates.
    """
    if not isinstance(value, (str, Path)):
        raise ValueError(f"executable must be {default!r} or an absolute path")
    value = str(value)
    if (not value or len(value.encode()) > 4096
            or any(ord(char) < 32 or 127 <= ord(char) <= 159 for char in value)):
        raise ValueError("invalid bounded executable selector")
    if value != default and (not Path(value).is_absolute()
            or str(Path(value)) != value or ".." in Path(value).parts):
        raise ValueError(f"executable must be {default!r} or an absolute path")
    return value


def resolve_command(value, *, default):
    """Resolve the caller's current PATH immediately before each process launch."""
    value = validate_command(value, default=default)
    found = shutil.which(executable_name(default)) if value == default else value
    if found is None or not is_executable(found):
        raise ValueError(f"executable unavailable: {value}")
    return os.path.abspath(found)


def validate_reader_codex(config):
    """A replaceable selector, optionally retaining an explicit legacy hash."""
    codex = validate_command(config.get("reader_codex"), default="codex")
    if "reader_codex_sha256" in config:
        if codex == "codex":
            raise ValueError("PATH reader_codex must omit reader_codex_sha256")
        digest = config["reader_codex_sha256"]
        if codex == "codex" or not isinstance(digest, str) or re.fullmatch(r"[0-9a-f]{64}", digest) is None:
            raise ValueError("invalid reader_codex_sha256")


def prepare_reader_codex(value="codex"):
    """Preserve launch intent; choosing a path does not request an edition pin."""
    value = validate_command(value, default="codex")
    if value != "codex":
        resolve_command(value, default="codex")
    return {"reader_codex": value}


def prepare_python_selector(value="python3", *, windows=None):
    """Keep POSIX PATH intent; preserve established setup-selected Windows Python.

    This is authoring only. Receipt rendering must never depend on the later
    machine's current interpreter. Explicit absolute selectors remain lexical.
    """
    value = validate_command(value, default="python3")
    windows = WINDOWS if windows is None else windows
    return sys.executable if windows and value == "python3" else value


def python_hook_argv(python, arguments, *, windows=None):
    """Launch supported Python dynamically without persisting its resolved edition.

    Explicit absolute selectors are invoked unchanged. POSIX's logical default
    probes a finite compatible interpreter set each launch, so an old system
    python3 cannot hide an installed supported interpreter.
    """
    python = validate_command(python, default="python3")
    windows = WINDOWS if windows is None else windows
    if python != "python3" or windows:
        return [executable_name(python, windows=windows) if python == "python3" else python, *arguments]
    script = python_path_script()
    return ["/bin/sh", "-c", script, "mneme-python", *arguments]


def find_executable(name, *, sibling_dir=None):
    """Prefer the packaged native executable, then a native PATH executable."""
    native = executable_name(name)
    if sibling_dir is not None:
        candidate = Path(sibling_dir) / native
        if is_executable(candidate):
            return str(candidate)
    result = shutil.which(native)
    return result if result is not None and is_executable(result) else None


def _directory(environment, fallback):
    value = os.environ.get(environment)
    path = Path(value) if value else fallback
    if not path.is_absolute():
        raise ValueError(f"{environment} must be absolute")
    return path / "mneme"


def default_config_dir():
    """Only the default for new configuration; never migrate/discover old data."""
    if os.environ.get("XDG_CONFIG_HOME"):
        return _directory("XDG_CONFIG_HOME", Path.home() / ".config")
    if WINDOWS:
        return _directory("APPDATA", Path.home() / "AppData" / "Roaming")
    return _directory("XDG_CONFIG_HOME", Path.home() / ".config")


def default_data_dir():
    """Local (not roaming) data default on Windows, XDG data default on Unix."""
    if os.environ.get("XDG_DATA_HOME"):
        return _directory("XDG_DATA_HOME", Path.home() / ".local" / "share")
    if WINDOWS:
        return _directory("LOCALAPPDATA", Path.home() / "AppData" / "Local")
    return _directory("XDG_DATA_HOME", Path.home() / ".local" / "share")


def _powershell_literal(value):
    return "'" + value.replace("'", "''") + "'"


def hook_command(argv, *, windows=None):
    """Render a complete command; append arguments to argv *before* rendering.

    Windows PowerShell 5.1's normal `& executable @args` can lose empty/quoted
    native arguments. ProcessStartInfo instead receives the exact CRT-quoted
    argument string, bypasses shell execution, inherits stdin/stdout/stderr and
    propagates the actual child exit code. Only fixed tokens and base64 reach
    Codex's outer shell; paths with %, !, &, apostrophes or Unicode remain data.
    Requires Windows PowerShell on PATH; no profile or interpolated source runs.
    """
    if (not isinstance(argv, (list, tuple)) or not argv
            or any(not isinstance(arg, str) or any(c in arg for c in "\x00\r\n") for arg in argv)
            or not argv[0]):
        raise ValueError("hook argv must contain bounded-line string arguments")
    windows = WINDOWS if windows is None else windows
    if not windows:
        return shlex.join(argv)
    if not argv[0].lower().endswith(".exe"):
        raise ValueError("Windows hooks require a native .exe interpreter")
    script = ("$ErrorActionPreference='Stop';"
              "$p=New-Object System.Diagnostics.ProcessStartInfo;"
              "$p.FileName=" + _powershell_literal(argv[0]) + ";"
              "$p.Arguments=" + _powershell_literal(subprocess.list2cmdline(argv[1:])) + ";"
              "$p.UseShellExecute=$false;"
              "$p.EnvironmentVariables['PYTHONUTF8']='1';"
              "$p.EnvironmentVariables['PYTHONIOENCODING']='utf-8';"
              "$c=[System.Diagnostics.Process]::Start($p);"
              "$c.WaitForExit();exit $c.ExitCode")
    encoded = base64.b64encode(script.encode("utf-16-le")).decode("ascii")
    command = "powershell.exe -NoProfile -NonInteractive -OutputFormat Text -EncodedCommand " + encoded
    # Codex can select cmd.exe (8,191-character command limit). Reserve room
    # for its fixed invocation/outer quote wrapper instead of a late shell error.
    if len(command) > 8191 - 64:
        raise ValueError("Windows hook command exceeds cmd.exe command length; use a shorter install path")
    return command


def checkpoint_command(argv, *, windows=None):
    """Agent-facing checkpoint prefix; Windows callers must label it PowerShell.

    Unlike hook_command this deliberately accepts a suffix, but *only* the
    checkpoint grammar: --outcome none/deferred or captured with 1..8 --id ULIDs.
    Fixed argv uses ProcessStartInfo to preserve JSON quotes on PowerShell 5.1.
    The checked ASCII-only suffix needs no second shell/CRT quoting layer.
    """
    windows = WINDOWS if windows is None else windows
    if not windows:
        return hook_command(argv, windows=False)
    # Reuse the input/native-executable checks, not the encoded envelope.
    if (not isinstance(argv, (list, tuple)) or not argv or not argv[0]
            or any(not isinstance(arg, str) or any(c in arg for c in "\x00\r\n") for arg in argv)
            or not argv[0].lower().endswith(".exe")):
        raise ValueError("Windows checkpoint needs native executable argv")
    script = ("& {$ErrorActionPreference='Stop';$s=$args -join ' ';"
              "if($s -cnotmatch '\\A--outcome (?:none|deferred|captured(?: --id [0-9A-HJKMNP-TV-Z]{26}){1,8})\\z')"
              "{throw 'Invalid checkpoint arguments'};"
              "$p=New-Object System.Diagnostics.ProcessStartInfo;"
              "$p.FileName=" + _powershell_literal(argv[0]) + ";"
              "$p.Arguments=" + _powershell_literal(subprocess.list2cmdline(argv[1:])) + "+' '+$s;"
              "$p.UseShellExecute=$false;"
              "$p.EnvironmentVariables['PYTHONUTF8']='1';"
              "$p.EnvironmentVariables['PYTHONIOENCODING']='utf-8';"
              "$c=[System.Diagnostics.Process]::Start($p);$c.WaitForExit();"
              "$global:LASTEXITCODE=$c.ExitCode;if($c.ExitCode){throw ('Checkpoint exited '+$c.ExitCode)}}")
    # The hook consumer applies its actual aggregate context budget before
    # spending a continuation token; the renderer does not invent another quota.
    return script
