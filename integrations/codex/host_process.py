"""Bounded local process primitives for Unix and native Windows.

``BoundedPipe`` owns raw binary stdin/stdout: do not mix it with buffered reads,
``communicate()``, or concurrent readers/writers. Deadlines are absolute monotonic
seconds. Read allocation is bounded by ``max_read_bytes``; frame/aggregate limits
remain the caller's responsibility. A failed write can be partial; never replay it.

Windows uses PIPE_NOWAIT, not selectors (which cannot select anonymous pipes) or
uncancellable I/O threads. ``tree=True`` starts suspended, assigns a noninherited
kill-on-close Job Object, then resumes the verified primary thread. No target code
runs before assignment. If the spawning Python process is killed in the narrow
CreateProcess-to-assignment interval, a suspended child can remain; this is not an
atomic create-in-job implementation. Ordinary errors clean up that child. Job
objects contain ordinary CreateProcess descendants, not processes launched through
external services/WMI. Detached owners must not use ``tree=True`` if they should
outlive their launcher.

Native API contracts:
https://learn.microsoft.com/en-us/windows/win32/api/namedpipeapi/nf-namedpipeapi-setnamedpipehandlestate
https://learn.microsoft.com/en-us/windows/win32/procthread/job-objects
"""

import ctypes
import functools
import math
import os
import re
import selectors
import signal
import subprocess
import time
import weakref

IS_WINDOWS = os.name == "nt"
_POLL_SECONDS = 0.01
_MAX_IO_CHUNK = 16 * 1024
_DEFAULT_MAX_READ_BYTES = 512 * 1024


def _remaining(deadline):
    if not isinstance(deadline, (int, float)) or not math.isfinite(deadline):
        raise ValueError("deadline must be finite monotonic seconds")
    remaining = deadline - time.monotonic()
    if remaining <= 0:
        raise TimeoutError("local process pipe deadline exceeded; operation was not retried")
    return remaining


def _pause(deadline):
    time.sleep(min(_POLL_SECONDS, _remaining(deadline)))


class BoundedPipe:
    """Single-owner, deadline-bounded binary pipes; close does not kill the child."""

    def __init__(self, process, *, max_read_bytes=_DEFAULT_MAX_READ_BYTES):
        if type(max_read_bytes) is not int or max_read_bytes <= 0:
            raise ValueError("max_read_bytes must be a positive integer")
        if process.stdin is None or process.stdout is None:
            raise ValueError("BoundedPipe needs piped stdin and stdout")
        if getattr(process, "text_mode", False):
            raise ValueError("BoundedPipe requires binary streams")
        self.process = process
        self.max_read_bytes = max_read_bytes
        self._closed = False
        self._windows = IS_WINDOWS
        self._read_fd = process.stdout.fileno()
        self._write_fd = process.stdin.fileno()
        if self._windows:
            self._api = _win()
            self._read_handle = self._api.pipe_handle(self._read_fd)
            self._write_handle = self._api.pipe_handle(self._write_fd)
            self._api.nonblocking(self._read_handle)
            self._api.nonblocking(self._write_handle)
        else:
            os.set_blocking(self._read_fd, False)
            os.set_blocking(self._write_fd, False)

    def _check_open(self):
        if self._closed:
            raise ValueError("local process pipes are closed")

    def read(self, max_bytes, deadline):
        """Read at most max_bytes, return b'' for EOF, or raise TimeoutError."""
        self._check_open()
        if type(max_bytes) is not int or max_bytes <= 0 or max_bytes > self.max_read_bytes:
            raise ValueError("read exceeds configured per-read byte limit")
        if self._windows:
            while True:
                _remaining(deadline)
                data = self._api.read_pipe(self._read_handle, max_bytes)
                if data is not None:
                    return data
                _pause(deadline)
        with selectors.DefaultSelector() as selector:
            selector.register(self._read_fd, selectors.EVENT_READ)
            while True:
                if not selector.select(_remaining(deadline)):
                    raise TimeoutError("local process pipe read timed out")
                try:
                    return os.read(self._read_fd, max_bytes)
                except BlockingIOError:
                    continue

    def write(self, data, deadline):
        """Write bytes by deadline; timeout/error may follow a partial write."""
        self._check_open()
        if not isinstance(data, (bytes, bytearray, memoryview)):
            raise TypeError("pipe data must be bytes-like")
        pending = memoryview(data).cast("B")
        if not pending:
            return
        if self._windows:
            chunk_size = _MAX_IO_CHUNK
            while pending:
                _remaining(deadline)
                count = self._api.write_pipe(self._write_handle, pending[:chunk_size])
                if count:
                    pending = pending[count:]
                    chunk_size = _MAX_IO_CHUNK
                else:
                    # PIPE_NOWAIT may successfully write zero bytes when the
                    # requested chunk does not fit. Reduce to fit a smaller
                    # pipe/free tail, then poll rather than spin on a full pipe.
                    previous_size = min(chunk_size, len(pending))
                    chunk_size = max(1, previous_size // 2)
                    if previous_size == 1:
                        _pause(deadline)
            return
        with selectors.DefaultSelector() as selector:
            selector.register(self._write_fd, selectors.EVENT_WRITE)
            while pending:
                if not selector.select(_remaining(deadline)):
                    raise TimeoutError("local process pipe write timed out")
                try:
                    count = os.write(self._write_fd, pending[:_MAX_IO_CHUNK])
                except BlockingIOError:
                    continue
                if count <= 0:
                    raise BrokenPipeError("local process input pipe closed")
                pending = pending[count:]

    def close(self):
        if self._closed:
            return
        self._closed = True
        for stream in (self.process.stdin, self.process.stdout):
            try:
                stream.close()
            except OSError:
                pass


# Win32 types have fixed widths even when tests import this module on Unix.
_DWORD = ctypes.c_uint32
_LONG = ctypes.c_int32
_HANDLE = ctypes.c_void_p
_BOOL = ctypes.c_int32
_FILETIME = _DWORD * 2


class _ThreadEntry(ctypes.Structure):
    _fields_ = [("dwSize", _DWORD), ("cntUsage", _DWORD),
                ("th32ThreadID", _DWORD), ("th32OwnerProcessID", _DWORD),
                ("tpBasePri", _LONG), ("tpDeltaPri", _LONG), ("dwFlags", _DWORD)]


class _JobBasicLimits(ctypes.Structure):
    _fields_ = [("PerProcessUserTimeLimit", ctypes.c_int64),
                ("PerJobUserTimeLimit", ctypes.c_int64), ("LimitFlags", _DWORD),
                ("MinimumWorkingSetSize", ctypes.c_size_t),
                ("MaximumWorkingSetSize", ctypes.c_size_t),
                ("ActiveProcessLimit", _DWORD), ("Affinity", ctypes.c_size_t),
                ("PriorityClass", _DWORD), ("SchedulingClass", _DWORD)]


class _JobLimits(ctypes.Structure):
    _fields_ = [("BasicLimitInformation", _JobBasicLimits),
                ("IoInfo", ctypes.c_uint64 * 6),
                ("ProcessMemoryLimit", ctypes.c_size_t),
                ("JobMemoryLimit", ctypes.c_size_t),
                ("PeakProcessMemoryUsed", ctypes.c_size_t),
                ("PeakJobMemoryUsed", ctypes.c_size_t)]


class _WindowsApi:
    """Small typed kernel32 boundary, loaded only on Windows."""

    def __init__(self):
        self.kernel = ctypes.WinDLL("kernel32", use_last_error=True)
        pointer = ctypes.c_void_p
        dword_pointer = ctypes.POINTER(_DWORD)
        self._bind("SetNamedPipeHandleState", _BOOL,
                   [_HANDLE, dword_pointer, dword_pointer, dword_pointer])
        self._bind("ReadFile", _BOOL, [_HANDLE, pointer, _DWORD, dword_pointer, pointer])
        self._bind("WriteFile", _BOOL, [_HANDLE, pointer, _DWORD, dword_pointer, pointer])
        self._bind("OpenProcess", _HANDLE, [_DWORD, _BOOL, _DWORD])
        self._bind("CloseHandle", _BOOL, [_HANDLE])
        self._bind("WaitForSingleObject", _DWORD, [_HANDLE, _DWORD])
        self._bind("QueryFullProcessImageNameW", _BOOL,
                   [_HANDLE, _DWORD, ctypes.c_wchar_p, dword_pointer])
        self._bind("GetProcessTimes", _BOOL, [_HANDLE, pointer, pointer, pointer, pointer])
        self._bind("TerminateProcess", _BOOL, [_HANDLE, _DWORD])
        self._bind("CreateJobObjectW", _HANDLE, [pointer, ctypes.c_wchar_p])
        self._bind("SetInformationJobObject", _BOOL, [_HANDLE, ctypes.c_int, pointer, _DWORD])
        self._bind("AssignProcessToJobObject", _BOOL, [_HANDLE, _HANDLE])
        self._bind("TerminateJobObject", _BOOL, [_HANDLE, _DWORD])
        self._bind("CreateToolhelp32Snapshot", _HANDLE, [_DWORD, _DWORD])
        self._bind("Thread32First", _BOOL, [_HANDLE, ctypes.POINTER(_ThreadEntry)])
        self._bind("Thread32Next", _BOOL, [_HANDLE, ctypes.POINTER(_ThreadEntry)])
        self._bind("OpenThread", _HANDLE, [_DWORD, _BOOL, _DWORD])
        self._bind("GetProcessIdOfThread", _DWORD, [_HANDLE])
        self._bind("ResumeThread", _DWORD, [_HANDLE])

    def _bind(self, name, result, arguments):
        function = getattr(self.kernel, name)
        function.restype = result
        function.argtypes = arguments

    @staticmethod
    def error():
        return ctypes.WinError(ctypes.get_last_error())

    def close(self, handle):
        if not self.kernel.CloseHandle(handle):
            raise self.error()

    @staticmethod
    def pipe_handle(fd):
        import msvcrt
        return msvcrt.get_osfhandle(fd)

    def nonblocking(self, handle):
        mode = _DWORD(1)  # PIPE_READMODE_BYTE | PIPE_NOWAIT
        if not self.kernel.SetNamedPipeHandleState(handle, ctypes.byref(mode), None, None):
            raise self.error()  # No blocking fallback.

    def read_pipe(self, handle, max_bytes):
        buffer = ctypes.create_string_buffer(max_bytes)
        read = _DWORD()
        if not self.kernel.ReadFile(handle, buffer, max_bytes, ctypes.byref(read), None):
            code = ctypes.get_last_error()
            if code == 232:  # ERROR_NO_DATA: connected pipe with nothing ready
                return None
            if code in (109, 233):  # ERROR_BROKEN_PIPE / PIPE_NOT_CONNECTED
                return b""
            raise self.error()
        return buffer.raw[:read.value]

    def write_pipe(self, handle, data):
        raw = bytes(data)  # Caller caps every allocation at _MAX_IO_CHUNK.
        buffer = ctypes.create_string_buffer(raw, len(raw))
        written = _DWORD()
        if not self.kernel.WriteFile(handle, buffer, len(raw), ctypes.byref(written), None):
            raise self.error()  # ERROR_NO_DATA on write means closed, not backpressure.
        return written.value

    def open_process(self, pid, *, terminate=False):
        rights = 0x1000 | 0x100000  # QUERY_LIMITED_INFORMATION | SYNCHRONIZE
        if terminate:
            rights |= 0x0001  # PROCESS_TERMINATE
        handle = self.kernel.OpenProcess(rights, False, pid)
        if not handle:
            if ctypes.get_last_error() == 87:  # ERROR_INVALID_PARAMETER: no such PID
                return None
            raise self.error()
        return handle

    def running(self, handle):
        result = self.kernel.WaitForSingleObject(handle, 0)
        if result == 258:  # WAIT_TIMEOUT
            return True
        if result == 0:  # WAIT_OBJECT_0
            return False
        raise self.error()

    def identity(self, handle, pid):
        if not self.running(handle):
            return None
        size = _DWORD(32768)
        image = ctypes.create_unicode_buffer(size.value)
        if not self.kernel.QueryFullProcessImageNameW(handle, 0, image, ctypes.byref(size)):
            raise self.error()
        created, exited, kernel, user = (_FILETIME() for _ in range(4))
        if not self.kernel.GetProcessTimes(handle, created, exited, kernel, user):
            raise self.error()
        if not self.running(handle):
            return None
        return {"platform": "windows", "pid": pid, "executable": image.value,
                "created": created[0] | (created[1] << 32)}

    def terminate(self, handle):
        if not self.kernel.TerminateProcess(handle, 1):
            error = self.error()
            if self.running(handle):
                raise error

    def create_job(self):
        job = self.kernel.CreateJobObjectW(None, None)  # not inheritable, unnamed
        if not job:
            raise self.error()
        limits = _JobLimits()
        limits.BasicLimitInformation.LimitFlags = 0x2000  # KILL_ON_JOB_CLOSE
        if not self.kernel.SetInformationJobObject(job, 9, ctypes.byref(limits), ctypes.sizeof(limits)):
            error = self.error()
            self.close(job)
            raise error
        return job

    def assign_job(self, job, process):
        if not self.kernel.AssignProcessToJobObject(job, int(process._handle)):
            raise self.error()

    def terminate_job(self, job):
        if not self.kernel.TerminateJobObject(job, 1):
            raise self.error()

    def resume_primary(self, process):
        # Popen closes the CREATE_SUSPENDED primary thread handle. The process
        # has not executed, so it must have exactly one thread. Never select an
        # arbitrary thread or resume a running process as a fallback.
        snapshot = self.kernel.CreateToolhelp32Snapshot(4, 0)  # SNAPTHREAD
        if snapshot == _HANDLE(-1).value:
            raise self.error()
        threads = []
        try:
            entry = _ThreadEntry()
            entry.dwSize = ctypes.sizeof(entry)
            present = self.kernel.Thread32First(snapshot, ctypes.byref(entry))
            while present:
                if entry.th32OwnerProcessID == process.pid:
                    threads.append(entry.th32ThreadID)
                entry.dwSize = ctypes.sizeof(entry)
                present = self.kernel.Thread32Next(snapshot, ctypes.byref(entry))
            if ctypes.get_last_error() != 18:  # ERROR_NO_MORE_FILES
                raise self.error()
        finally:
            self.close(snapshot)
        if len(threads) != 1:
            raise OSError("cannot identify unique suspended primary thread")
        # SUSPEND_RESUME | QUERY_LIMITED_INFORMATION; no inheritable handle.
        thread = self.kernel.OpenThread(0x0002 | 0x0800, False, threads[0])
        if not thread:
            raise self.error()
        try:
            if (self.kernel.GetProcessIdOfThread(thread) != process.pid
                    or not self.running(int(process._handle))):
                raise OSError("suspended child identity changed before resume")
            if self.kernel.ResumeThread(thread) != 1:
                raise OSError("unexpected suspended primary-thread state")
        finally:
            self.close(thread)


_WINDOWS_API = None


def _win():
    global _WINDOWS_API
    if _WINDOWS_API is None:
        _WINDOWS_API = _WindowsApi()
    return _WINDOWS_API


def _close_streams(process):
    for name in ("stdin", "stdout", "stderr"):
        stream = getattr(process, name, None)
        if stream is not None:
            try:
                stream.close()
            except OSError:
                pass


def spawn(argv, *, detached=False, tree=False, **kwargs):
    """Popen with explicit detach/tree ownership; no shell or inherited-handle tricks."""
    if kwargs.get("shell"):
        raise ValueError("local process spawning does not accept shell=True")
    if not IS_WINDOWS:
        if detached or tree:
            if kwargs.get("start_new_session") is False:
                raise ValueError("detached/tree process requires a new session")
            kwargs["start_new_session"] = True
        process = subprocess.Popen(argv, **kwargs)
        process._mneme_tree = tree
        return process
    if kwargs.pop("start_new_session", False):
        detached = True
    flags = kwargs.pop("creationflags", 0)
    if flags & 0x00000004:  # CREATE_SUSPENDED is owned here, not by callers.
        raise ValueError("CREATE_SUSPENDED is managed by spawn(tree=True)")
    if detached:
        flags |= 0x00000008 | 0x00000200  # DETACHED_PROCESS | NEW_PROCESS_GROUP
    elif tree:
        flags |= 0x08000000  # CREATE_NO_WINDOW
    if not tree:
        return subprocess.Popen(argv, creationflags=flags, **kwargs)
    if kwargs.get("close_fds") is False:
        raise ValueError("owned process trees require close_fds=True")
    kwargs["close_fds"] = True
    api = _win()
    job = api.create_job()
    process = None
    try:
        process = subprocess.Popen(argv, creationflags=flags | 0x00000004, **kwargs)
        api.assign_job(job, process)
        process._mneme_job = job
        process._mneme_job_finalizer = weakref.finalize(process, api.close, job)
        api.resume_primary(process)
        return process
    except BaseException:
        try:
            if process is not None:
                try:
                    process.kill()
                    process.wait(timeout=2)
                finally:
                    _close_streams(process)
                    finalizer = getattr(process, "_mneme_job_finalizer", None)
                    if finalizer is not None:
                        finalizer.detach()
        finally:
            api.close(job)
        raise



def _signal_owned_group(process, sig):
    try:
        os.killpg(process.pid, sig)
    except ProcessLookupError:
        return
    except PermissionError:
        # Darwin can report EPERM while an exited group consists only of
        # zombies. Reaping our leader is not sufficient: descendants killed by
        # SIGTERM may still await launchd's reap when we send final SIGKILL.
        # Allow a short, bounded disappearance window, but suppress only ESRCH,
        # never a persistent permission failure or EPERM with a live leader.
        if process.poll() is None:
            raise
        deadline = time.monotonic() + 0.1
        while True:
            remaining = deadline - time.monotonic()
            if remaining <= 0:
                raise
            time.sleep(min(_POLL_SECONDS, remaining))
            try:
                os.killpg(process.pid, sig)
                return
            except ProcessLookupError:
                return
            except PermissionError:
                continue


def terminate_tree(process, *, timeout=2):
    """Terminate an owned tree (or just an ungrouped process), with finite waits."""
    if not isinstance(timeout, (int, float)) or not math.isfinite(timeout) or timeout < 0:
        raise ValueError("termination timeout must be finite and nonnegative")
    if IS_WINDOWS:
        job = getattr(process, "_mneme_job", None)
        if job is not None:
            try:
                _win().terminate_job(job)
            finally:
                process._mneme_job_finalizer()
                process._mneme_job = None
        elif process.poll() is None:
            process.terminate()
        process.wait(timeout=timeout)
        return
    grouped = getattr(process, "_mneme_tree", False)
    try:
        if grouped:
            _signal_owned_group(process, signal.SIGTERM)
        elif process.poll() is None:
            process.terminate()
    except ProcessLookupError:
        pass
    try:
        process.wait(timeout=timeout)
    except subprocess.TimeoutExpired:
        if not grouped:
            process.kill()
    finally:
        # The leader can exit while a descendant ignores SIGTERM. Kill the
        # owned group even after the leader has exited, not only on wait timeout.
        if grouped:
            _signal_owned_group(process, signal.SIGKILL)
            process._mneme_tree = False
    process.wait(timeout=max(timeout, 0.1))


def _valid_pid(pid):
    return type(pid) is int and 0 < pid <= 0xffffffff


def process_alive(pid):
    """True/False/None for alive/absent/unverifiable; never signal Windows PIDs."""
    if not _valid_pid(pid):
        return False
    if IS_WINDOWS:
        api = _win()
        handle = None
        try:
            handle = api.open_process(pid)
            return api.running(handle) if handle is not None else False
        except OSError:
            return None
        finally:
            if handle is not None:
                api.close(handle)
    try:
        os.kill(pid, 0)
        return True
    except ProcessLookupError:
        return False
    except OSError:
        return None


def process_identity(pid):
    """Return an executable/start identity, or None for absent/unverifiable."""
    if not _valid_pid(pid):
        return None
    if IS_WINDOWS:
        api = _win()
        handle = None
        try:
            handle = api.open_process(pid)
            return api.identity(handle, pid) if handle is not None else None
        except OSError:
            return None
        finally:
            if handle is not None:
                api.close(handle)
    # Unix consumers keep their existing whole-argv service validation. This
    # portable identity is only additional evidence, not an atomic signal guard.
    try:
        result = subprocess.run(["ps", "-ww", "-p", str(pid), "-o", "lstart=", "-o", "comm="],
                                capture_output=True, text=True, check=False, timeout=2)
        line = result.stdout.strip()
        if result.returncode != 0 or len(line) < 25:
            return None
        return {"platform": "posix", "pid": pid, "created": line[:24],
                "executable": line[24:].strip()}
    except (OSError, subprocess.TimeoutExpired):
        return None



def spawned_identity(process):
    """Identity of this exact Popen child, not a newly opened numeric PID.

    Native Windows service startup uses the retained CreateProcess handle so a
    fast exit/PID reuse cannot bind recorded state to another process.
    """
    if not IS_WINDOWS:
        return process_identity(process.pid)
    try:
        return _win().identity(int(process._handle), process.pid)
    except OSError:
        return None

def terminate_verified(pid, identity):
    """Signal only a matching identity. Windows uses one handle for check + kill.

    Returns False for absent/mismatch, True after signalling; OSError means the
    operation could not be verified/completed. Unix's check/signal is not atomic:
    existing consumers should retain their stricter platform-specific guard.
    """
    if not _valid_pid(pid) or not isinstance(identity, dict) or identity.get("pid") != pid:
        return False
    if IS_WINDOWS:
        api = _win()
        handle = api.open_process(pid, terminate=True)
        if handle is None:
            return False
        try:
            if api.identity(handle, pid) != identity:
                return False
            api.terminate(handle)
            return True
        finally:
            api.close(handle)
    current = process_identity(pid)
    if current is None:
        if process_alive(pid) is False:
            return False
        raise OSError("process identity unavailable; refusing termination")
    if current != identity:
        return False
    try:
        os.kill(pid, signal.SIGTERM)
    except ProcessLookupError:
        return False
    return True


_WINDOWS_BOOT_QUERY = (
    "$ErrorActionPreference='Stop'; "
    "$v=(Get-CimInstance -ClassName Win32_OperatingSystem -Property LastBootUpTime).LastBootUpTime; "
    "if ($null -eq $v) { exit 2 }; "
    "[Console]::Out.Write($v.ToUniversalTime().ToString('o', "
    "[Globalization.CultureInfo]::InvariantCulture))"
)


@functools.lru_cache(maxsize=1)
def boot_identity():
    """Best-effort Windows boot witness; unavailable is None, never an estimate.

    The documented CIM LastBootUpTime is formatted as exact invariant UTC text,
    with a two-second process/I/O budget and a 128-byte aggregate output limit.
    Cached per process to avoid repeated WMI startup. Unix callers retain their
    existing /proc or sysctl witnesses. This is a boot-time witness, not a boot
    GUID: it is not protection against deliberate clock/state manipulation.

    https://learn.microsoft.com/en-us/windows/win32/cimwin32prov/win32-operatingsystem
    """
    if not IS_WINDOWS:
        return None
    root = os.environ.get("SystemRoot")
    if not root or not os.path.isabs(root):
        return None
    powershell = os.path.join(root, "System32", "WindowsPowerShell", "v1.0", "powershell.exe")
    process = pipe = None
    deadline = time.monotonic() + 2
    try:
        process = spawn([powershell, "-NoLogo", "-NoProfile", "-NonInteractive",
                         "-Command", _WINDOWS_BOOT_QUERY], tree=True,
                        stdin=subprocess.PIPE, stdout=subprocess.PIPE,
                        stderr=subprocess.DEVNULL, bufsize=0)
        pipe = BoundedPipe(process, max_read_bytes=129)
        output = b""
        while True:
            part = pipe.read(129 - len(output), deadline)
            if not part:
                break
            output += part
            if len(output) > 128:
                return None
        if process.wait(timeout=_remaining(deadline)) != 0:
            return None
        value = output.decode("ascii")
        if not re.fullmatch(r"[0-9]{4}-[0-9]{2}-[0-9]{2}T[0-9]{2}:[0-9]{2}:[0-9]{2}\.[0-9]{7}Z", value):
            return None
        return "windows-cim:" + value
    except (OSError, ValueError, UnicodeError, subprocess.SubprocessError):
        return None
    finally:
        if pipe is not None:
            pipe.close()
        elif process is not None:
            _close_streams(process)
        if process is not None:
            try:
                terminate_tree(process, timeout=0.25)
            except (OSError, subprocess.SubprocessError):
                # The kill-on-close job remains the final cleanup backstop.
                pass
