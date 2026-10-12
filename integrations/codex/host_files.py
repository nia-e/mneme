"""Checked local files, private metadata and advisory leases on Unix and Windows.

Windows privacy is an owner/SYSTEM/Administrators DACL, never a chmod claim.
Reparse points are rejected, including directory components used by these APIs.
Publication uses a flushed payload and a write-through Windows rename; a bare
Windows directory fsync is deliberately *not* represented as a successful no-op.
"""
from __future__ import annotations

import contextlib
import ctypes
import errno
import functools
import os
from pathlib import Path
import stat
import sys
import tempfile

WINDOWS = os.name == "nt"


def _descriptor(file):
    return file if isinstance(file, int) else file.fileno()


class _Overlapped(ctypes.Structure):
    _fields_ = [("Internal", ctypes.c_size_t), ("InternalHigh", ctypes.c_size_t),
                ("Offset", ctypes.c_uint32), ("OffsetHigh", ctypes.c_uint32),
                ("hEvent", ctypes.c_void_p)]


class _FileInfo(ctypes.Structure):
    _fields_ = [("attributes", ctypes.c_uint32), ("creation", ctypes.c_uint32 * 2),
                ("access", ctypes.c_uint32 * 2), ("write", ctypes.c_uint32 * 2),
                ("volume", ctypes.c_uint32), ("size_high", ctypes.c_uint32),
                ("size_low", ctypes.c_uint32), ("links", ctypes.c_uint32),
                ("index_high", ctypes.c_uint32), ("index_low", ctypes.c_uint32)]


class _SecurityAttributes(ctypes.Structure):
    _fields_ = [("length", ctypes.c_uint32), ("descriptor", ctypes.c_void_p),
                ("inherit", ctypes.c_int)]


class _Acl(ctypes.Structure):
    _fields_ = [("revision", ctypes.c_ubyte), ("reserved", ctypes.c_ubyte),
                ("size", ctypes.c_uint16), ("count", ctypes.c_uint16),
                ("reserved2", ctypes.c_uint16)]


class _WindowsFiles:
    """Small ctypes binding, loaded only on native Windows.

    API contracts: CreateFileW/FILE_FLAG_OPEN_REPARSE_POINT; LockFileEx;
    GetSecurityInfo/SetSecurityInfo; MoveFileExW/MOVEFILE_WRITE_THROUGH.
    https://learn.microsoft.com/windows/win32/api/fileapi/nf-fileapi-createfilew
    https://learn.microsoft.com/windows/win32/api/aclapi/nf-aclapi-getsecurityinfo
    https://learn.microsoft.com/windows/win32/api/winbase/nf-winbase-movefileexw
    """
    def __init__(self):
        self.kernel = ctypes.WinDLL("kernel32", use_last_error=True)
        self.security = ctypes.WinDLL("advapi32", use_last_error=True)
        p, u, b = ctypes.c_void_p, ctypes.c_uint32, ctypes.c_int
        self._bind(self.kernel, "CreateFileW", [ctypes.c_wchar_p, u, u, p, u, u, p], p)
        self._bind(self.kernel, "CloseHandle", [p], b)
        self._bind(self.kernel, "GetFileType", [p], u)
        self._bind(self.kernel, "GetFileInformationByHandle", [p, ctypes.POINTER(_FileInfo)], b)
        self._bind(self.kernel, "GetCurrentProcess", [], p)
        self._bind(self.kernel, "LocalFree", [p], p)
        self._bind(self.kernel, "CreateDirectoryW", [ctypes.c_wchar_p, p], b)
        self._bind(self.kernel, "MoveFileExW", [ctypes.c_wchar_p, ctypes.c_wchar_p, u], b)
        self._bind(self.kernel, "LockFileEx", [p, u, u, u, u, ctypes.POINTER(_Overlapped)], b)
        self._bind(self.kernel, "UnlockFileEx", [p, u, u, u, ctypes.POINTER(_Overlapped)], b)
        self._bind(self.security, "OpenProcessToken", [p, u, ctypes.POINTER(p)], b)
        self._bind(self.security, "GetTokenInformation", [p, u, p, u, ctypes.POINTER(u)], b)
        self._bind(self.security, "ConvertSidToStringSidW", [p, ctypes.POINTER(p)], b)
        self._bind(self.security, "ConvertStringSecurityDescriptorToSecurityDescriptorW",
                   [ctypes.c_wchar_p, u, ctypes.POINTER(p), ctypes.POINTER(u)], b)
        self._bind(self.security, "GetSecurityDescriptorDacl", [p, ctypes.POINTER(b), ctypes.POINTER(p), ctypes.POINTER(b)], b)
        self._bind(self.security, "GetSecurityDescriptorControl", [p, ctypes.POINTER(ctypes.c_uint16), ctypes.POINTER(u)], b)
        self._bind(self.security, "GetSecurityInfo", [p, u, u, ctypes.POINTER(p), p, ctypes.POINTER(p), p, ctypes.POINTER(p)], u)
        self._bind(self.security, "SetSecurityInfo", [p, u, u, p, p, p, p], u)
        self._bind(self.security, "GetAce", [p, u, ctypes.POINTER(p)], b)

    @staticmethod
    def _bind(library, name, args, result):
        function = getattr(library, name)
        function.argtypes, function.restype = args, result

    @staticmethod
    def error(code=None):
        return ctypes.WinError(ctypes.get_last_error() if code is None else code)

    def sid_string(self, sid):
        out = ctypes.c_void_p()
        if not self.security.ConvertSidToStringSidW(sid, ctypes.byref(out)):
            raise self.error()
        try:
            return ctypes.wstring_at(out)
        finally:
            self.kernel.LocalFree(out)

    @functools.cached_property
    def user_sid(self):
        token, needed = ctypes.c_void_p(), ctypes.c_uint32()
        if not self.security.OpenProcessToken(self.kernel.GetCurrentProcess(), 0x0008, ctypes.byref(token)):
            raise self.error()
        try:
            self.security.GetTokenInformation(token, 1, None, 0, ctypes.byref(needed))
            if not needed.value:
                raise self.error()
            data = ctypes.create_string_buffer(needed.value)
            if not self.security.GetTokenInformation(token, 1, data, needed, ctypes.byref(needed)):
                raise self.error()
            return self.sid_string(ctypes.cast(data, ctypes.POINTER(ctypes.c_void_p))[0])
        finally:
            self.kernel.CloseHandle(token)

    @contextlib.contextmanager
    def private_security(self, directory=False):
        inheritance = "OICI" if directory else ""
        sddl = f"O:{self.user_sid}D:P" + "".join(f"(A;{inheritance};FA;;;{sid})" for sid in (self.user_sid, "SY", "BA"))
        descriptor = ctypes.c_void_p()
        if not self.security.ConvertStringSecurityDescriptorToSecurityDescriptorW(sddl, 1, ctypes.byref(descriptor), None):
            raise self.error()
        attributes = _SecurityAttributes(ctypes.sizeof(_SecurityAttributes), descriptor, 0)
        try:
            yield attributes
        finally:
            self.kernel.LocalFree(descriptor)

    def info(self, handle):
        value = _FileInfo()
        if self.kernel.GetFileType(handle) != 1 or not self.kernel.GetFileInformationByHandle(handle, ctypes.byref(value)):
            raise ValueError("not_disk_file")
        if value.attributes & 0x400:
            raise ValueError("reparse_point_not_allowed")
        return value

    def open_handle(self, path, *, access=0x80000000, disposition=3, directory=False,
                    private=False, pin=False):
        flags = 0x00200000 | (0x02000000 if directory else 0)
        with self.private_security(directory) if private else contextlib.nullcontext(None) as security:
            handle = self.kernel.CreateFileW(str(path), access, 3 if pin else 7,
                ctypes.byref(security) if security is not None else None, disposition, flags, None)
        if handle == ctypes.c_void_p(-1).value:
            raise self.error()
        try:
            info = self.info(handle)
            if bool(info.attributes & 0x10) != directory:
                raise ValueError("not_directory" if directory else "not_regular_file")
            return handle, info
        except BaseException:
            self.kernel.CloseHandle(handle)
            raise

    @contextlib.contextmanager
    def parents(self, path):
        """Pin each ordinary directory without FILE_SHARE_DELETE during path use."""
        path = Path(os.path.abspath(path))
        handles = []
        try:
            for parent in reversed(path.parents):
                handle, _ = self.open_handle(parent, directory=True, access=0x80, pin=True)
                handles.append(handle)
            yield path
        finally:
            for handle in reversed(handles):
                self.kernel.CloseHandle(handle)

    def check_owner(self, handle):
        owner, descriptor = ctypes.c_void_p(), ctypes.c_void_p()
        code = self.security.GetSecurityInfo(handle, 1, 0x1, ctypes.byref(owner), None,
                                            None, None, ctypes.byref(descriptor))
        if code:
            raise self.error(code)
        try:
            if not owner.value or self.sid_string(owner) != self.user_sid:
                raise PermissionError("file_not_owned_by_current_user")
        finally:
            self.kernel.LocalFree(descriptor)

    def check_private(self, handle):
        owner, dacl, descriptor = ctypes.c_void_p(), ctypes.c_void_p(), ctypes.c_void_p()
        code = self.security.GetSecurityInfo(handle, 1, 0x5, ctypes.byref(owner), None,
            ctypes.byref(dacl), None, ctypes.byref(descriptor))
        if code:
            raise self.error(code)
        try:
            if not owner.value or self.sid_string(owner) != self.user_sid or not dacl.value:
                raise PermissionError("file_is_not_private_to_current_user")
            control, revision = ctypes.c_uint16(), ctypes.c_uint32()
            if not self.security.GetSecurityDescriptorControl(descriptor, ctypes.byref(control), ctypes.byref(revision)):
                raise self.error()
            if not control.value & 0x1000:  # SE_DACL_PROTECTED
                raise PermissionError("private_dacl_must_not_inherit")
            allowed = {self.user_sid, "S-1-5-18", "S-1-5-32-544"}
            for index in range(ctypes.cast(dacl, ctypes.POINTER(_Acl)).contents.count):
                ace = ctypes.c_void_p()
                if not self.security.GetAce(dacl, index, ctypes.byref(ace)):
                    raise self.error()
                header = ctypes.string_at(ace, 4)
                if header[1] & 0x08:  # INHERIT_ONLY_ACE does not grant this object access.
                    continue
                # Only ordinary allow/deny ACEs are supported; object/callback
                # ACEs are not assumed harmless under an unfamiliar contract.
                if header[0] == 1:
                    continue
                if header[0] != 0 or self.sid_string(ace.value + 8) not in allowed:
                    raise PermissionError("file_acl_grants_other_principals")
        finally:
            self.kernel.LocalFree(descriptor)


@functools.lru_cache(maxsize=1)
def _windows():
    return _WindowsFiles()


def open_regular(path, flags=os.O_RDONLY, mode=0o600, *, single_link=True):
    """Open a non-reparse regular file and return an owned, binary descriptor.

    Truncation occurs only after type/link validation. On Windows all existing
    ancestors are pinned against rename while opening the leaf. The returned fd
    owns the admitted identity; it does not promise the pathname never changes.
    """
    if WINDOWS:
        import msvcrt
        api = _windows()
        access = 0xC0000000 if flags & os.O_RDWR else 0x40000000 if flags & os.O_WRONLY else 0x80000000
        if flags & os.O_CREAT:
            access |= 0x20000  # READ_CONTROL: verify actual private ACL before use.
        disposition = 1 if flags & os.O_CREAT and flags & os.O_EXCL else 4 if flags & os.O_CREAT else 3
        with api.parents(path) as absolute:
            handle, info = api.open_handle(absolute, access=access, disposition=disposition, private=bool(flags & os.O_CREAT))
        try:
            if single_link and info.links != 1:
                raise ValueError("multiply_linked_file")
            if flags & os.O_CREAT:
                api.check_private(handle)
            descriptor = msvcrt.open_osfhandle(handle, (flags & (os.O_RDONLY | os.O_WRONLY | os.O_RDWR | os.O_APPEND)) | os.O_BINARY)
        except BaseException:
            api.kernel.CloseHandle(handle)
            raise
    else:
        descriptor = os.open(path, (flags & ~os.O_TRUNC) | os.O_NOFOLLOW | os.O_NONBLOCK, mode)
    try:
        info = os.fstat(descriptor)
        if not stat.S_ISREG(info.st_mode):
            raise ValueError("not_regular_file")
        if single_link and info.st_nlink != 1:
            raise ValueError("multiply_linked_file")
        if flags & os.O_TRUNC:
            os.ftruncate(descriptor, 0)
        return descriptor
    except BaseException:
        os.close(descriptor)
        raise


def open_directory(path):
    """Return a no-follow directory descriptor; caller closes it with os.close."""
    if not WINDOWS:
        return os.open(path, os.O_RDONLY | os.O_DIRECTORY | os.O_NOFOLLOW)
    import msvcrt
    api = _windows()
    with api.parents(path) as absolute:
        handle, _ = api.open_handle(absolute, directory=True)
    try:
        return msvcrt.open_osfhandle(handle, os.O_RDONLY | os.O_BINARY)
    except BaseException:
        api.kernel.CloseHandle(handle)
        raise


def lock_file(file, *, exclusive=True, blocking=True):
    """Lock the fd's entire identity; nonblocking contention is BlockingIOError."""
    fd = _descriptor(file)
    if not WINDOWS:
        import fcntl
        fcntl.flock(fd, (fcntl.LOCK_EX if exclusive else fcntl.LOCK_SH) | (0 if blocking else fcntl.LOCK_NB))
        return
    import msvcrt
    api = _windows()
    if not api.kernel.LockFileEx(msvcrt.get_osfhandle(fd), (2 if exclusive else 0) | (0 if blocking else 1),
                               0, 0xFFFFFFFF, 0xFFFFFFFF, ctypes.byref(_Overlapped())):
        code = ctypes.get_last_error()
        if code == 33:
            raise BlockingIOError(errno.EWOULDBLOCK, "file lock is held")
        raise api.error(code)


def unlock_file(file):
    fd = _descriptor(file)
    if not WINDOWS:
        import fcntl
        fcntl.flock(fd, fcntl.LOCK_UN)
        return
    import msvcrt
    api = _windows()
    if not api.kernel.UnlockFileEx(msvcrt.get_osfhandle(fd), 0, 0xFFFFFFFF, 0xFFFFFFFF, ctypes.byref(_Overlapped())):
        raise api.error()


def check_private_fd(file, *, directory=False):
    """Check privacy/type/link count on the already admitted handle, not a path."""
    fd = _descriptor(file)
    info = os.fstat(fd)
    if not (stat.S_ISDIR(info.st_mode) if directory else stat.S_ISREG(info.st_mode)):
        raise ValueError("not_directory" if directory else "not_regular_file")
    if not directory and info.st_nlink != 1:
        raise ValueError("multiply_linked_file")
    if WINDOWS:
        import msvcrt
        api = _windows()
        handle = msvcrt.get_osfhandle(fd)
        api.info(handle)  # rejects reparse points on this handle as well
        api.check_private(handle)
    elif info.st_uid != os.getuid() or info.st_mode & 0o077:
        raise PermissionError("file_is_not_private_to_current_user")


def check_private(path, *, directory=False):
    """Reject unsafe type/link/ownership/access; never repair permissions on read."""
    fd = open_directory(path) if directory else open_regular(path)
    try:
        check_private_fd(fd, directory=directory)
    finally:
        os.close(fd)


def check_private_directory(path):
    check_private(path, directory=True)


def set_private(path, *, directory=False, mode=None):
    """Explicitly narrow an admitted object's permissions to its current user."""
    permission = (0o700 if directory else 0o600) if mode is None else mode
    if permission not in (0o600, 0o700) or (directory and permission != 0o700):
        raise ValueError("private mode must be 0600 or executable/directory 0700")
    if not WINDOWS:
        fd = open_directory(path) if directory else open_regular(path)
        try:
            if os.fstat(fd).st_uid != os.getuid():
                raise PermissionError("file_not_owned_by_current_user")
            os.fchmod(fd, permission)
        finally:
            os.close(fd)
        return
    api = _windows()
    with api.parents(path) as absolute:
        handle, info = api.open_handle(absolute, directory=directory, access=0x60000 | 0x80)
        try:
            if not directory and info.links != 1:
                raise ValueError("multiply_linked_file")
            api.check_owner(handle)
            # Existing directories are narrowed as one object, not as an ACL
            # propagation request over unrelated existing descendants.
            with api.private_security(False) as security:
                present, defaulted, dacl = ctypes.c_int(), ctypes.c_int(), ctypes.c_void_p()
                if not api.security.GetSecurityDescriptorDacl(security.descriptor, ctypes.byref(present), ctypes.byref(dacl), ctypes.byref(defaulted)):
                    raise api.error()
                code = api.security.SetSecurityInfo(handle, 1, 0x80000004, None, None, dacl, None)
                if code:
                    raise api.error(code)
            api.check_private(handle)
        finally:
            api.kernel.CloseHandle(handle)


def ensure_private_directory(path, *, parents=True, exist_ok=True):
    """Create privately, or verify an existing directory without broad chmod."""
    path = Path(path)
    try:
        if parents and not path.parent.exists():
            ensure_private_directory(path.parent, parents=True)
        if WINDOWS:
            api = _windows()
            with api.parents(path) as absolute, api.private_security(True) as security:
                if not api.kernel.CreateDirectoryW(str(absolute), ctypes.byref(security)):
                    raise api.error()
        else:
            path.mkdir(mode=0o700)
    except FileExistsError:
        if not exist_ok:
            raise
    check_private_directory(path)


def ensure_state_directory(path, *, parents=True, exist_ok=True):
    """Create private state, retaining safe historical Unix directory modes.

    Earlier Unix workers created some intermediate state directories as 0755.
    Reading those directories does not authorize chmod or invalidate their
    existing state. Require current ownership and no other-principal write,
    while preserving readable/traversable mode bits. Windows has no prior Unix
    mode-bit contract: actual private ACL validation remains mandatory there.
    """
    if WINDOWS:
        return ensure_private_directory(path, parents=parents, exist_ok=exist_ok)
    try:
        ensure_private_directory(path, parents=parents, exist_ok=False)
    except FileExistsError:
        if not exist_ok:
            raise
        fd = open_directory(path)
        try:
            info = os.fstat(fd)
            if info.st_uid != os.getuid() or info.st_mode & 0o022:
                raise PermissionError("state_directory_has_untrusted_owner_or_write_access")
        finally:
            os.close(fd)


def sync_directory(path):
    """Unix directory durability barrier; unsupported alone on Windows.

    Use atomic_write for a Windows write-through publication. Silently ignoring
    directory fsync failure would incorrectly claim the Unix durability contract.
    """
    if WINDOWS:
        raise NotImplementedError("Windows directory fsync is unavailable; use write-through publication")
    fd = open_directory(path)
    try:
        os.fsync(fd)
    finally:
        os.close(fd)


def sync_file(path):
    """Flush an admitted regular single-link file without rewriting its bytes."""
    # Windows FlushFileBuffers needs GENERIC_WRITE even for a flush-only call.
    fd = open_regular(path, os.O_RDWR if WINDOWS else os.O_RDONLY)
    try:
        os.fsync(fd)
    finally:
        os.close(fd)


def _replace_synced_file(source, target):
    if WINDOWS:
        api = _windows()
        if not api.kernel.MoveFileExW(str(Path(source).absolute()), str(Path(target).absolute()), 0x1 | 0x8):
            raise api.error()
    else:
        os.replace(source, target)
        sync_directory(Path(target).parent)


def replace_file(source, target):
    """Flush/publish a caller-owned regular stage beside a regular/absent target.

    Replacing an existing regular target is deliberate caller authority. The
    source's bytes and ACL are preserved. A failure after rename is ambiguous;
    callers must retain operation-bound evidence rather than blindly retry.
    """
    source, target = Path(source), Path(target)
    if source.absolute().parent != target.absolute().parent:
        raise ValueError("file publication stage must be beside its target")
    api = _windows() if WINDOWS else None
    with api.parents(target) if WINDOWS else contextlib.nullcontext():
        fd = open_regular(source, os.O_RDWR if WINDOWS else os.O_RDONLY)
        try:
            check_private_fd(fd)
            os.fsync(fd)
        finally:
            os.close(fd)
        try:
            fd = open_regular(target)
        except FileNotFoundError:
            pass
        else:
            os.close(fd)
        _replace_synced_file(source, target)


def rename_no_replace(source, target):
    """Atomically publish a file/directory without overwriting *any* target.

    Caller must flush its closed, bounded payload inventory first. This helper
    publishes the namespace and its durability barrier, not a recursive fsync.
    Cross-volume moves are refused (no copy/delete fallback). A reported failure
    after the rename may be ambiguous and is not permission for a blind retry.
    """
    source, target = Path(source).absolute(), Path(target).absolute()
    if WINDOWS:
        api = _windows()
        with api.parents(source), api.parents(target):
            directory = source.is_dir()
            handle, info = api.open_handle(source, directory=directory, access=0x80)
            api.kernel.CloseHandle(handle)
            if not directory and info.links != 1:
                raise ValueError("multiply_linked_file")
            if not api.kernel.MoveFileExW(str(source), str(target), 0x8):
                raise api.error()
        return
    fd = open_directory(source) if stat.S_ISDIR(source.lstat().st_mode) else open_regular(source)
    os.close(fd)
    # os.rename can replace an empty directory, so it cannot establish no-clobber.
    library = ctypes.CDLL(None, use_errno=True)
    if sys.platform == "darwin":
        function = library.renamex_np
        function.argtypes = [ctypes.c_char_p, ctypes.c_char_p, ctypes.c_uint32]
        function.restype = ctypes.c_int
        result = function(os.fsencode(source), os.fsencode(target), 0x4)  # RENAME_EXCL
    elif sys.platform.startswith("linux") and hasattr(library, "renameat2"):
        function = library.renameat2
        function.argtypes = [ctypes.c_int, ctypes.c_char_p, ctypes.c_int, ctypes.c_char_p, ctypes.c_uint32]
        function.restype = ctypes.c_int
        result = function(-100, os.fsencode(source), -100, os.fsencode(target), 1)  # RENAME_NOREPLACE
    else:
        raise NotImplementedError("atomic no-replace rename unavailable on this host")
    if result:
        code = ctypes.get_errno()
        raise OSError(code, os.strerror(code), str(target))
    sync_directory(target.parent)
    if source.parent != target.parent:
        sync_directory(source.parent)


def atomic_write(path, data, *, mode=0o600):
    """Publish complete private bytes in the same directory; refuse unsafe leaves.

    Does not create parents or overwrite nonregular/multiply-linked targets.
    Callers retain authority over whether replacing a *regular* target is allowed.
    """
    if not isinstance(data, bytes):
        raise TypeError("atomic_write requires bytes")
    path = Path(path)
    api = _windows() if WINDOWS else None
    with api.parents(path) if WINDOWS else contextlib.nullcontext(path):
        try:
            existing = open_regular(path)
        except FileNotFoundError:
            pass
        else:
            os.close(existing)
        # Windows creation uses a private descriptor immediately, rather than
        # temporarily publishing secret bytes through an inherited broad DACL.
        fd = None
        owns_temporary = False
        if WINDOWS:
            import secrets
            temporary = path.parent / (".mneme-publish-" + secrets.token_hex(16))
        else:
            fd, name = tempfile.mkstemp(prefix=".mneme-publish-", dir=path.parent)
            temporary = Path(name)
            owns_temporary = True
        try:
            if WINDOWS:
                fd = open_regular(temporary, os.O_WRONLY | os.O_CREAT | os.O_EXCL, mode)
                owns_temporary = True
            else:
                os.fchmod(fd, mode)
            with os.fdopen(fd, "wb") as stream:
                fd = None
                stream.write(data)
                stream.flush()
                os.fsync(stream.fileno())
            _replace_synced_file(temporary, path)
        finally:
            if fd is not None:
                os.close(fd)
            if owns_temporary:
                try:
                    temporary.unlink()
                except FileNotFoundError:
                    pass
