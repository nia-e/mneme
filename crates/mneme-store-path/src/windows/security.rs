//! Handle-based ACL admission and private creation descriptors.
//!
//! This is a local-user boundary, not protection from administrators: the current
//! process user, LocalSystem and Builtin Administrators are trusted. Generic
//! private objects and the main database require current-user ownership; only
//! SQLite sidecars also admit Builtin Administrators ownership. We reject ACL
//! forms we do not understand rather than reproduce the full access algorithm.
//! Database writes require a default owner of current user or Administrators,
//! and reject an impersonating caller. The runtime must not change security
//! context between admission and subsequent SQLite file creation.
//!
//! Win32 allocation/layout contracts used below:
//! - <https://learn.microsoft.com/en-us/windows/win32/api/aclapi/nf-aclapi-getsecurityinfo>
//! - <https://learn.microsoft.com/en-us/windows/win32/api/securitybaseapi/nf-securitybaseapi-gettokeninformation>
//! - <https://learn.microsoft.com/en-us/windows/win32/api/securitybaseapi/nf-securitybaseapi-getace>
//! - <https://learn.microsoft.com/en-us/windows/win32/api/winnt/ns-winnt-access_allowed_ace>
//! - <https://learn.microsoft.com/en-us/windows/win32/api/sddl/nf-sddl-convertstringsecuritydescriptortosecuritydescriptorw>
//! - <https://learn.microsoft.com/en-us/windows/win32/secauthz/dacl-for-a-new-object>
//! - <https://learn.microsoft.com/en-us/windows/win32/secauthz/ace-inheritance-rules>
//! - <https://learn.microsoft.com/en-us/windows/win32/secauthz/owner-of-a-new-object>

use std::ffi::c_void;
use std::fs::File;
use std::io;
use std::mem::{MaybeUninit, align_of, offset_of, size_of};
use std::os::windows::io::{AsRawHandle, FromRawHandle, OwnedHandle};
use std::ptr::{self, NonNull};

use windows_sys::Win32::Foundation::{ERROR_NO_TOKEN, GENERIC_EXECUTE, GENERIC_READ, LocalFree};
use windows_sys::Win32::Security::Authorization::{
    ConvertSidToStringSidW, ConvertStringSecurityDescriptorToSecurityDescriptorW, GetSecurityInfo,
    SDDL_REVISION_1, SE_FILE_OBJECT,
};
use windows_sys::Win32::Security::{
    ACCESS_ALLOWED_ACE, ACE_HEADER, ACL, DACL_SECURITY_INFORMATION, EqualSid, GetAce,
    GetSecurityDescriptorDacl, GetSecurityDescriptorOwner, GetTokenInformation, INHERIT_ONLY_ACE,
    IsValidAcl, IsValidSid, IsWellKnownSid, OBJECT_INHERIT_ACE, OWNER_SECURITY_INFORMATION,
    PSECURITY_DESCRIPTOR, PSID, SECURITY_MAX_SID_SIZE, TOKEN_OWNER, TOKEN_QUERY, TOKEN_USER,
    TokenOwner, TokenUser, WinBuiltinAdministratorsSid, WinCreatorOwnerSid, WinLocalSystemSid,
};
use windows_sys::Win32::Storage::FileSystem::{FILE_GENERIC_EXECUTE, FILE_GENERIC_READ};
use windows_sys::Win32::System::SystemServices::{ACCESS_ALLOWED_ACE_TYPE, ACCESS_DENIED_ACE_TYPE};
use windows_sys::Win32::System::Threading::{
    GetCurrentProcess, GetCurrentThread, OpenProcessToken, OpenThreadToken,
};

const PARENT_READ_ONLY: u32 =
    FILE_GENERIC_READ | FILE_GENERIC_EXECUTE | GENERIC_READ | GENERIC_EXECUTE;

/// Require current-user ownership and no access grants to untrusted SIDs.
pub(super) fn require_private(file: &File, directory: bool) -> io::Result<()> {
    let descriptor = SecurityDescriptor::for_file(file)?;
    descriptor
        .admit(AdmissionPolicy::PrivateObject)
        .map_err(|error| {
            io::Error::new(
                error.kind(),
                format!(
                    "{} is not private: {error}",
                    if directory { "directory" } else { "file" }
                ),
            )
        })
}

/// A lease's parent may be readable, but untrusted users must not modify it.
/// This checks the parent itself; it does not validate a chain of ancestors.
pub(super) fn require_safe_parent(file: &File) -> io::Result<()> {
    SecurityDescriptor::for_file(file)?.admit(AdmissionPolicy::SafeParent)
}

/// Check a database parent's effective permissions and direct-file inheritance.
/// SQLite opens sidecars without an explicit security descriptor, unlike Mneme's
/// protected private-file creation. Its parent must therefore supply a private
/// file-inheritable DACL; otherwise Windows can use the creator's default DACL.
/// Dynamic CREATOR OWNER templates are deliberately unsupported here; database
/// parent ACLs must name their supported trustees explicitly.
pub(super) fn require_private_children(file: &File) -> io::Result<()> {
    require_process_creation_owner()?;
    SecurityDescriptor::for_file(file)?.admit(AdmissionPolicy::PrivateFileChildren)
}

/// SQLite default-security sidecars may be Administrators-owned when created by
/// an elevated token. That principal is already trusted for full access. Admit
/// it independently of the current token's default owner so recovery also works
/// after restarting unelevated. Generic private files and the main DB keep the
/// stricter current-user ownership policy.
pub(super) fn require_private_database_sidecar(file: &File) -> io::Result<()> {
    SecurityDescriptor::for_file(file)?.admit(AdmissionPolicy::DatabaseSidecar)
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum AdmissionPolicy {
    PrivateObject,
    SafeParent,
    PrivateFileChildren,
    DatabaseSidecar,
}

pub(super) struct PrivateSecurity {
    descriptor: SecurityDescriptor,
}

impl PrivateSecurity {
    pub(super) fn new(directory: bool) -> io::Result<Self> {
        let user = TokenSid::user()?.sid_string()?;
        let inherit = if directory { "OICI" } else { "" };
        // P disables inherited grants from the parent. Set the owner explicitly
        // as elevated processes can otherwise default to the Administrators SID.
        let sddl = format!(
            "O:{user}D:P(A;{inherit};FA;;;{user})(A;{inherit};FA;;;SY)(A;{inherit};FA;;;BA)"
        );
        Ok(Self {
            descriptor: SecurityDescriptor::from_sddl(&sddl)?,
        })
    }

    /// Borrow the descriptor until this value is dropped. Win32 creation APIs
    /// read, but do not take ownership of, this allocation.
    pub(super) fn as_ptr(&self) -> PSECURITY_DESCRIPTOR {
        self.descriptor.0.0.as_ptr()
    }
}

struct LocalAllocation(
    // Safety invariant: one exclusively owned, non-null LocalAlloc-family
    // allocation. It is freed exactly once by Drop, never by a borrowed pointer.
    NonNull<c_void>,
);

impl LocalAllocation {
    /// # Safety
    /// The caller transfers exclusive ownership of a non-null allocation whose
    /// API contract requires LocalFree. No other owner may free that allocation.
    unsafe fn take(pointer: *mut c_void) -> io::Result<Self> {
        NonNull::new(pointer)
            .map(Self)
            .ok_or_else(|| io::Error::other("Windows returned a null allocation"))
    }
}

impl Drop for LocalAllocation {
    fn drop(&mut self) {
        // SAFETY: The field invariant provides the original LocalAlloc-family
        // pointer, still live and exclusively owned. No borrowed pointer escapes
        // its owner as a safe reference; Drop frees it once and performs no reads.
        unsafe { LocalFree(self.0.as_ptr()) };
    }
}

struct SecurityDescriptor(
    // Safety invariant: the allocation contains a valid self-relative security
    // descriptor from GetSecurityInfo or SDDL conversion. This module never
    // mutates it; all component pointers are borrowed while it remains alive.
    LocalAllocation,
);

impl SecurityDescriptor {
    fn for_file(file: &File) -> io::Result<Self> {
        let mut descriptor = ptr::null_mut();
        // SAFETY: File owns the live handle throughout the call. The requested
        // object kind is a file/directory; descriptor is an exclusive writable
        // output slot. Other outputs are optional and null. On success Win32
        // returns an independently allocated descriptor released with LocalFree.
        let error = unsafe {
            GetSecurityInfo(
                file.as_raw_handle(),
                SE_FILE_OBJECT,
                OWNER_SECURITY_INFORMATION | DACL_SECURITY_INFORMATION,
                ptr::null_mut(),
                ptr::null_mut(),
                ptr::null_mut(),
                ptr::null_mut(),
                &mut descriptor,
            )
        };
        if error != 0 {
            return Err(io::Error::from_raw_os_error(error as i32));
        }
        // SAFETY: Successful GetSecurityInfo transferred its unique LocalFree
        // allocation to descriptor. No other value owns it, including file.
        Ok(Self(unsafe { LocalAllocation::take(descriptor)? }))
    }

    fn from_sddl(sddl: &str) -> io::Result<Self> {
        if sddl.contains('\0') {
            return Err(io::Error::new(io::ErrorKind::InvalidInput, "NUL in SDDL"));
        }
        let wide: Vec<u16> = sddl.encode_utf16().chain(Some(0)).collect();
        let mut descriptor = ptr::null_mut();
        // SAFETY: wide is an initialized, NUL-terminated UTF-16 buffer kept alive
        // for this non-retaining call. descriptor is an exclusive output slot;
        // size is optional. Success transfers a valid self-relative descriptor.
        if unsafe {
            ConvertStringSecurityDescriptorToSecurityDescriptorW(
                wide.as_ptr(),
                SDDL_REVISION_1,
                &mut descriptor,
                ptr::null_mut(),
            )
        } == 0
        {
            return Err(io::Error::last_os_error());
        }
        // SAFETY: The SDDL API requires LocalFree for this newly returned, unique
        // allocation. Ownership is transferred into this guard immediately.
        Ok(Self(unsafe { LocalAllocation::take(descriptor)? }))
    }

    fn admit(&self, policy: AdmissionPolicy) -> io::Result<()> {
        let current = TokenSid::user()?;
        let mut owner = ptr::null_mut();
        let mut defaulted = 0;
        // SAFETY: The descriptor invariant guarantees the live valid descriptor;
        // outputs are exclusive writable slots. The returned owner borrows it.
        if unsafe { GetSecurityDescriptorOwner(self.0.0.as_ptr(), &mut owner, &mut defaulted) } == 0
        {
            return Err(io::Error::last_os_error());
        }
        if owner.is_null() {
            return Err(refused("missing owner SID"));
        }
        let owner_admitted = if policy == AdmissionPolicy::DatabaseSidecar {
            // SAFETY: The descriptor query returned a complete valid owner SID
            // in self's still-live immutable allocation; current retains its SID.
            unsafe { database_owner_allowed(owner, &current) }
        } else {
            // SAFETY: The successful descriptor query returned an initialized
            // owner SID in self's allocation; current owns its valid user SID.
            // EqualSid only reads both and retains neither pointer.
            unsafe { EqualSid(owner, current.sid) != 0 }
        };
        if !owner_admitted {
            return Err(refused("owner is not admitted for this object"));
        }

        let mut present = 0;
        let mut dacl = ptr::null_mut();
        // SAFETY: The descriptor is still alive and unmodified; each output is
        // a distinct writable slot. The returned ACL borrows this descriptor.
        if unsafe {
            GetSecurityDescriptorDacl(self.0.0.as_ptr(), &mut present, &mut dacl, &mut defaulted)
        } == 0
        {
            return Err(io::Error::last_os_error());
        }
        if present == 0 || dacl.is_null() {
            return Err(refused("absent or null DACL grants unrestricted access"));
        }
        // SAFETY: GetSecurityDescriptorDacl returned a non-null ACL backed by
        // self's live descriptor. No code mutates or frees it during inspection.
        unsafe { check_dacl(dacl, &current, policy) }
    }
}

/// # Safety
/// `dacl` must point to a complete initialized, DWORD-aligned ACL in a live
/// allocation, including all AclSize bytes, and remain immutable throughout this
/// call. Its ACEs must also satisfy Windows' DWORD-alignment requirement.
unsafe fn check_dacl(
    dacl: *const ACL,
    current: &TokenSid,
    policy: AdmissionPolicy,
) -> io::Result<()> {
    // SAFETY: The caller supplies a complete non-null ACL and keeps its whole
    // allocation readable and immutable. IsValidAcl does not retain the pointer.
    if unsafe { IsValidAcl(dacl) } == 0 {
        return Err(refused("invalid DACL"));
    }
    // SAFETY: The caller supplies an initialized ACL header. Reading it unaligned
    // copies only integer fields, without borrowing or moving the allocation.
    let acl = unsafe { dacl.read_unaligned() };
    let mut trusted_file_allow = false;
    for index in 0..u32::from(acl.AceCount) {
        let mut ace = ptr::null_mut();
        // SAFETY: IsValidAcl established that AceCount entries fit AclSize;
        // index is below that count. The ACL is still live and immutable and ace
        // is an exclusive writable output slot. Success borrows an ACE from it.
        if unsafe { GetAce(dacl, index, &mut ace) } == 0 {
            return Err(io::Error::last_os_error());
        }
        if ace.is_null() {
            return Err(refused("null ACE"));
        }
        let Some(offset) = ace.addr().checked_sub(dacl.addr()) else {
            return Err(refused("ACE outside DACL"));
        };
        if offset < size_of::<ACL>()
            || offset
                .checked_add(size_of::<ACE_HEADER>())
                .is_none_or(|end| end > usize::from(acl.AclSize))
        {
            return Err(refused("ACE header outside DACL"));
        }
        // SAFETY: GetAce borrows from the validated ACL allocation. The range
        // check covers its initialized ACE_HEADER inside that allocation;
        // read_unaligned copies its integer fields only.
        let header = unsafe { ace.cast::<ACE_HEADER>().read_unaligned() };
        if usize::from(header.AceSize) < size_of::<ACE_HEADER>()
            || offset + usize::from(header.AceSize) > usize::from(acl.AclSize)
        {
            return Err(refused("ACE extent outside DACL"));
        }
        let flags = u32::from(header.AceFlags);
        let effective = flags & INHERIT_ONLY_ACE == 0;
        // OI becomes effective on an immediate noncontainer child even when IO
        // or NP is set. CI without OI does not affect a direct child file.
        let file_inheritable =
            policy == AdmissionPolicy::PrivateFileChildren && flags & OBJECT_INHERIT_ACE != 0;
        // Other callers create protected children explicitly, so their current-
        // object check must still permit harmless inherit-only templates.
        if !effective && !file_inheritable {
            continue;
        }
        match u32::from(header.AceType) {
            ACCESS_DENIED_ACE_TYPE => continue,
            ACCESS_ALLOWED_ACE_TYPE => {}
            _ => return Err(refused("unsupported ACE type in DACL")),
        }
        const SID_OFFSET: usize = offset_of!(ACCESS_ALLOWED_ACE, SidStart);
        const SID_HEADER_BYTES: usize = 8;
        if usize::from(header.AceSize) < SID_OFFSET + SID_HEADER_BYTES {
            return Err(refused("truncated allowed ACE"));
        }
        // SAFETY: This is the ordinary allow layout. Its checked AceSize covers
        // ACCESS_ALLOWED_ACE, and IsValidAcl/GetAce established the ACE's range
        // inside the live ACL. read_unaligned copies initialized integer fields.
        let allowed = unsafe { ace.cast::<ACCESS_ALLOWED_ACE>().read_unaligned() };
        // SAFETY: SID_OFFSET is inside the checked ACE range, in the same live
        // allocation; the offset is tiny and fits isize. The resulting SID is
        // DWORD-aligned by the Windows ACE layout and contains its 8-byte header.
        let sid = unsafe { ace.cast::<u8>().add(SID_OFFSET) };
        // SAFETY: The checked SID header contains this initialized byte at index
        // one. The allocation stays live and immutable for this read.
        let subauthorities = unsafe { sid.add(1).read() };
        let sid_bytes = SID_HEADER_BYTES + usize::from(subauthorities) * size_of::<u32>();
        if sid_bytes > SECURITY_MAX_SID_SIZE as usize
            || SID_OFFSET + sid_bytes > usize::from(header.AceSize)
        {
            return Err(refused("truncated or oversized ACE SID"));
        }
        // SAFETY: The entire SID extent derived from its subauthority count now
        // fits the initialized ACE, and the SID is aligned for the Win32 API.
        // IsValidSid checks revision/count without modifying or retaining it.
        if unsafe { IsValidSid(sid.cast()) } == 0 {
            return Err(refused("invalid ACE SID"));
        }
        // SAFETY: IsValidSid and the checked extent establish a complete valid
        // SID. current retains its token allocation. All three APIs only read
        // these SIDs and retain neither pointer.
        let trusted = unsafe {
            EqualSid(sid.cast(), current.sid) != 0
                || IsWellKnownSid(sid.cast(), WinLocalSystemSid) != 0
                || IsWellKnownSid(sid.cast(), WinBuiltinAdministratorsSid) != 0
        };
        if allowed.Mask == 0 {
            continue;
        }
        if effective
            && !trusted
            && (matches!(
                policy,
                AdmissionPolicy::PrivateObject | AdmissionPolicy::DatabaseSidecar
            ) || allowed.Mask & !PARENT_READ_ONLY != 0)
        {
            return Err(refused("DACL grants access to an untrusted SID"));
        }
        if file_inheritable {
            if !trusted {
                // SAFETY: The validated SID remains in the unchanged, live ACL
                // allocation. IsWellKnownSid reads it without retaining it.
                if unsafe { IsWellKnownSid(sid.cast(), WinCreatorOwnerSid) } != 0 {
                    return Err(refused(
                        "file inheritance uses unsupported CREATOR OWNER; use explicit user SID grants",
                    ));
                }
                return Err(refused(
                    "DACL grants inherited file access to an untrusted SID",
                ));
            }
            trusted_file_allow = true;
        }
    }
    if policy == AdmissionPolicy::PrivateFileChildren && !trusted_file_allow {
        return Err(refused(
            "no trusted file-inheritable allow grant; creator default DACL is not admitted",
        ));
    }
    Ok(())
}

/// # Safety
/// `owner` must point to a complete, initialized, valid SID that remains readable
/// and immutable for this call. The pointer is neither modified nor retained.
unsafe fn database_owner_allowed(owner: PSID, current: &TokenSid) -> bool {
    // SAFETY: The caller supplies the complete owner SID and its lifetime;
    // TokenSid retains the complete current user SID. Both Win32 functions only
    // read these valid SIDs and retain neither pointer.
    unsafe {
        EqualSid(owner, current.sid) != 0 || IsWellKnownSid(owner, WinBuiltinAdministratorsSid) != 0
    }
}

struct TokenSid {
    // Safety invariant: this unmodified allocation retains the initialized
    // TOKEN_USER or TOKEN_OWNER and the SID pointed to by sid; moving Vec does not move its
    // allocation. No method resizes it or exposes mutable access.
    _buffer: Vec<MaybeUninit<usize>>,
    // Safety invariant: a valid token SID inside _buffer, live until Drop.
    sid: PSID,
}

#[derive(Clone, Copy)]
enum TokenSidKind {
    User,
    Owner,
}

fn process_token() -> io::Result<OwnedHandle> {
    let mut token = ptr::null_mut();
    // SAFETY: GetCurrentProcess has no arguments and returns a borrowed
    // pseudo-handle for this process. It must not be closed by this code.
    let process = unsafe { GetCurrentProcess() };
    // SAFETY: process is the valid current-process pseudo-handle; token is
    // an exclusive output slot. Success returns a newly owned token handle.
    if unsafe { OpenProcessToken(process, TOKEN_QUERY, &mut token) } == 0 {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: Successful OpenProcessToken returns a unique live handle
    // requiring CloseHandle. OwnedHandle takes exactly that ownership.
    Ok(unsafe { OwnedHandle::from_raw_handle(token) })
}

fn require_process_creation_owner() -> io::Result<()> {
    // SAFETY: This argument-free API returns a borrowed current-thread
    // pseudo-handle. It is valid for this call and is never owned or closed.
    let thread = unsafe { GetCurrentThread() };
    let mut impersonation = ptr::null_mut();
    // SAFETY: thread is a valid current-thread handle, and impersonation is an
    // exclusive output slot. OpenAsSelf uses process-level access checking.
    // Success transfers a token handle that must be closed with CloseHandle.
    if unsafe { OpenThreadToken(thread, TOKEN_QUERY, 1, &mut impersonation) } != 0 {
        // SAFETY: OpenThreadToken just transferred this live, uniquely owned
        // handle. OwnedHandle closes it once, including on the return below.
        let _token = unsafe { OwnedHandle::from_raw_handle(impersonation) };
        return Err(refused(
            "database writes do not support thread impersonation",
        ));
    }
    let error = io::Error::last_os_error();
    if error.raw_os_error() != Some(ERROR_NO_TOKEN as i32) {
        return Err(error);
    }
    let token = process_token()?;
    let user = TokenSid::query(&token, TokenSidKind::User)?;
    let owner = TokenSid::query(&token, TokenSidKind::Owner)?;
    // SAFETY: Both token queries own complete initialized SIDs and keep their
    // buffers alive throughout this read-only owner-policy comparison.
    if !unsafe { database_owner_allowed(owner.sid, &user) } {
        return Err(refused(
            "database file default owner is neither the user nor Builtin Administrators; use an ordinary user token",
        ));
    }
    Ok(())
}

impl TokenSid {
    fn user() -> io::Result<Self> {
        Self::query(&process_token()?, TokenSidKind::User)
    }

    fn query(token: &OwnedHandle, kind: TokenSidKind) -> io::Result<Self> {
        let (class, header_bytes) = match kind {
            TokenSidKind::User => (TokenUser, size_of::<TOKEN_USER>()),
            TokenSidKind::Owner => (TokenOwner, size_of::<TOKEN_OWNER>()),
        };
        const BYTES: usize = size_of::<TOKEN_USER>() + SECURITY_MAX_SID_SIZE as usize;
        const WORDS: usize = BYTES.div_ceil(size_of::<usize>());
        const {
            assert!(align_of::<TOKEN_USER>() <= align_of::<usize>());
            assert!(align_of::<TOKEN_OWNER>() <= align_of::<usize>());
            assert!(size_of::<TOKEN_OWNER>() <= size_of::<TOKEN_USER>());
        };
        let mut buffer = vec![MaybeUninit::<usize>::uninit(); WORDS];
        let capacity = (buffer.len() * size_of::<usize>()) as u32;
        let mut written = 0;
        // SAFETY: token is a live owned handle. Lack of query rights produces an
        // OS error, never a buffer access assumption. buffer is uniquely writable,
        // word-aligned for both possible headers, and capacity is its exact byte
        // size. The distinct return-length slot receives the initialized size.
        // Success initializes the selected header plus its SID in this buffer;
        // no pointer into this buffer is retained by the OS.
        if unsafe {
            GetTokenInformation(
                token.as_raw_handle(),
                class,
                buffer.as_mut_ptr().cast(),
                capacity,
                &mut written,
            )
        } == 0
        {
            return Err(io::Error::last_os_error());
        }
        if written < header_bytes as u32 || written > capacity {
            return Err(io::Error::other("invalid token SID result size"));
        }
        let sid = match kind {
            TokenSidKind::User => {
                // SAFETY: The TokenUser query initialized a complete TOKEN_USER
                // header. The checked size covers it; repr(C) pointer/integer
                // fields are Copy. buffer retains the pointed-to SID allocation.
                unsafe { buffer.as_ptr().cast::<TOKEN_USER>().read_unaligned() }
                    .User
                    .Sid
            }
            TokenSidKind::Owner => {
                // SAFETY: The TokenOwner query initialized a complete TOKEN_OWNER
                // header, covered by the size check. Its Copy raw pointer borrows
                // the SID in buffer, which is retained without reallocation.
                unsafe { buffer.as_ptr().cast::<TOKEN_OWNER>().read_unaligned() }.Owner
            }
        };
        if sid.is_null() {
            return Err(io::Error::other("missing token SID"));
        }
        Ok(Self {
            _buffer: buffer,
            sid,
        })
    }

    fn sid_string(&self) -> io::Result<String> {
        let mut text = ptr::null_mut();
        // SAFETY: The TokenSid invariant supplies a live valid SID. text is
        // an exclusive output slot; success allocates a NUL-terminated UTF-16
        // SID string and transfers ownership, requiring LocalFree.
        if unsafe { ConvertSidToStringSidW(self.sid, &mut text) } == 0 {
            return Err(io::Error::last_os_error());
        }
        // SAFETY: The conversion just transferred this unique LocalFree-family
        // allocation. The guard remains live while text is read below.
        let _allocation = unsafe { LocalAllocation::take(text.cast())? };
        let mut units = Vec::new();
        loop {
            // SAFETY: ConvertSidToStringSidW guarantees a NUL-terminated UTF-16
            // string. All previous units were non-NUL, so the next unit (possibly
            // the terminator) lies in that initialized allocation. A SID has at
            // most 15 u32 subauthorities, so this byte offset fits isize. No code
            // mutates/frees the allocation while this guard is alive.
            let unit = unsafe { text.add(units.len()).read() };
            if unit == 0 {
                break;
            }
            units.push(unit);
        }
        String::from_utf16(&units).map_err(|_| io::Error::other("invalid SID string"))
    }
}

fn refused(reason: &str) -> io::Error {
    io::Error::new(io::ErrorKind::PermissionDenied, reason)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn descriptor(dacl: &str) -> SecurityDescriptor {
        let user = TokenSid::user().unwrap().sid_string().unwrap();
        SecurityDescriptor::from_sddl(&format!("O:{user}{dacl}")).unwrap()
    }

    #[test]
    fn generated_private_descriptors_admit_current_user_and_system_trustees() {
        for directory in [false, true] {
            PrivateSecurity::new(directory)
                .unwrap()
                .descriptor
                .admit(AdmissionPolicy::PrivateObject)
                .unwrap();
        }
    }

    #[test]
    fn untrusted_read_access_is_only_allowed_for_parent_directories() {
        let read = descriptor("D:P(A;;GR;;;WD)");
        read.admit(AdmissionPolicy::SafeParent).unwrap();
        assert_eq!(
            read.admit(AdmissionPolicy::PrivateObject)
                .unwrap_err()
                .kind(),
            io::ErrorKind::PermissionDenied
        );
    }

    #[test]
    fn zero_mask_does_not_grant_access() {
        let descriptor = descriptor("D:P(A;;0x0;;;WD)");
        descriptor.admit(AdmissionPolicy::PrivateObject).unwrap();
        descriptor.admit(AdmissionPolicy::SafeParent).unwrap();
    }

    #[test]
    fn untrusted_mutation_and_unknown_rights_are_refused_on_parents() {
        for rights in [
            "GW",
            "GA",
            "0x2",
            "0x4",
            "0x10",
            "0x40",
            "0x100",
            "0x10000",
            "0x40000",
            "0x80000",
            "0x2000000",
            "0x800000",
        ] {
            let descriptor = descriptor(&format!("D:P(A;;{rights};;;WD)"));
            assert!(
                descriptor.admit(AdmissionPolicy::SafeParent).is_err(),
                "{rights}"
            );
        }
    }

    #[test]
    fn absent_and_null_dacls_are_refused_but_empty_dacl_is_not_unrestricted() {
        for dacl in ["", "D:NO_ACCESS_CONTROL"] {
            let descriptor = descriptor(dacl);
            assert!(
                descriptor.admit(AdmissionPolicy::PrivateObject).is_err(),
                "{dacl}"
            );
            assert!(
                descriptor.admit(AdmissionPolicy::SafeParent).is_err(),
                "{dacl}"
            );
        }
        descriptor("D:P")
            .admit(AdmissionPolicy::PrivateObject)
            .unwrap();
    }

    #[test]
    fn an_administrator_owner_is_not_the_process_user() {
        let descriptor = SecurityDescriptor::from_sddl("O:BAD:P(A;;FA;;;BA)").unwrap();
        assert!(descriptor.admit(AdmissionPolicy::PrivateObject).is_err());
        assert!(descriptor.admit(AdmissionPolicy::SafeParent).is_err());
    }

    #[test]
    fn deny_entries_do_not_excuse_untrusted_grants() {
        descriptor("D:P(D;;FA;;;WD)")
            .admit(AdmissionPolicy::PrivateObject)
            .unwrap();
        let descriptor = descriptor("D:P(D;;FA;;;WD)(A;;FA;;;WD)");
        assert!(descriptor.admit(AdmissionPolicy::PrivateObject).is_err());
    }

    #[test]
    fn unfamiliar_ace_forms_fail_closed() {
        // A nonempty object GUID prevents SDDL conversion from reducing this to
        // an ordinary allow ACE, even though the requested rights are read-only.
        let descriptor = descriptor("D:P(OA;;GR;00000000-0000-0000-0000-000000000001;;WD)");
        assert!(descriptor.admit(AdmissionPolicy::PrivateObject).is_err());
        assert!(descriptor.admit(AdmissionPolicy::SafeParent).is_err());
    }

    #[test]
    fn inherit_only_entries_do_not_grant_access_to_current_object() {
        for trustee in ["WD", "CO"] {
            let descriptor = descriptor(&format!("D:P(A;OICIIO;FA;;;{trustee})"));
            descriptor.admit(AdmissionPolicy::PrivateObject).unwrap();
            descriptor.admit(AdmissionPolicy::SafeParent).unwrap();
        }
    }

    #[test]
    fn sqlite_parent_checks_inherit_only_file_grants_including_no_propagate() {
        for flags in ["OIIO", "OICIIO", "OIIONP"] {
            for rights in ["GR", "GW"] {
                let descriptor = descriptor(&format!("D:P(A;OI;FA;;;SY)(A;{flags};{rights};;;WD)"));
                descriptor.admit(AdmissionPolicy::SafeParent).unwrap();
                assert!(
                    descriptor
                        .admit(AdmissionPolicy::PrivateFileChildren)
                        .is_err()
                );
            }
        }
    }

    #[test]
    fn sqlite_parent_refuses_effective_world_read_inherited_by_files() {
        let descriptor = descriptor("D:P(A;OI;FA;;;SY)(A;OI;GR;;;WD)");
        descriptor.admit(AdmissionPolicy::SafeParent).unwrap();
        assert!(
            descriptor
                .admit(AdmissionPolicy::PrivateFileChildren)
                .is_err()
        );
    }

    #[test]
    fn sqlite_parent_requires_positive_trusted_file_inheritance() {
        for dacl in [
            "D:P",
            "D:P(A;;FA;;;SY)",
            "D:P(A;CIIO;FA;;;SY)",
            "D:P(A;OIIO;0x0;;;SY)",
            "D:P(D;OI;FA;;;WD)",
        ] {
            let descriptor = descriptor(dacl);
            descriptor.admit(AdmissionPolicy::SafeParent).unwrap();
            assert!(
                descriptor
                    .admit(AdmissionPolicy::PrivateFileChildren)
                    .is_err(),
                "{dacl} must not fall back to an unchecked token default DACL"
            );
        }
        PrivateSecurity::new(true)
            .unwrap()
            .descriptor
            .admit(AdmissionPolicy::PrivateFileChildren)
            .unwrap();
    }

    #[test]
    fn sqlite_parent_ignores_container_only_templates_for_direct_files() {
        for ace in ["(A;CIIO;FA;;;WD)", "(A;CI;GR;;;WD)"] {
            descriptor(&format!("D:P(A;OI;FA;;;SY){ace}"))
                .admit(AdmissionPolicy::PrivateFileChildren)
                .unwrap();
        }
        // CI without IO is still effective on the parent: this write grant is
        // refused even though it would not reach an immediate child file.
        assert!(
            descriptor("D:P(A;OI;FA;;;SY)(A;CI;GW;;;WD)")
                .admit(AdmissionPolicy::PrivateFileChildren)
                .is_err()
        );
    }

    #[test]
    fn sqlite_parent_refuses_unknown_file_inheritable_ace_forms() {
        let descriptor =
            descriptor("D:P(A;OI;FA;;;SY)(OA;OIIO;GR;00000000-0000-0000-0000-000000000001;;WD)");
        descriptor.admit(AdmissionPolicy::SafeParent).unwrap();
        assert!(
            descriptor
                .admit(AdmissionPolicy::PrivateFileChildren)
                .is_err()
        );
    }

    #[test]
    fn sqlite_parent_does_not_assume_creator_owner_means_token_user() {
        let descriptor = descriptor("D:P(A;OI;FA;;;SY)(A;OIIO;FA;;;CO)");
        descriptor.admit(AdmissionPolicy::SafeParent).unwrap();
        assert!(
            descriptor
                .admit(AdmissionPolicy::PrivateFileChildren)
                .unwrap_err()
                .to_string()
                .contains("CREATOR OWNER")
        );
    }

    #[test]
    fn inherited_deny_does_not_excuse_untrusted_allow() {
        let descriptor = descriptor("D:P(D;OIIO;FA;;;WD)(A;OI;FA;;;SY)(A;OIIO;GR;;;WD)");
        descriptor.admit(AdmissionPolicy::SafeParent).unwrap();
        assert!(
            descriptor
                .admit(AdmissionPolicy::PrivateFileChildren)
                .is_err()
        );
    }

    #[test]
    fn administrator_owner_is_admitted_only_for_database_sidecars() {
        let descriptor = SecurityDescriptor::from_sddl("O:BAD:P(A;;FA;;;BA)").unwrap();
        descriptor.admit(AdmissionPolicy::DatabaseSidecar).unwrap();
        assert!(descriptor.admit(AdmissionPolicy::PrivateObject).is_err());
        assert!(descriptor.admit(AdmissionPolicy::SafeParent).is_err());
        assert!(
            descriptor
                .admit(AdmissionPolicy::PrivateFileChildren)
                .is_err()
        );
    }

    #[test]
    fn sidecar_admission_does_not_relax_untrusted_owners_or_grants() {
        // Builtin Users cannot be a process TokenUser, so this is an untrusted
        // owner even when the test happens to run as LocalSystem.
        let foreign = SecurityDescriptor::from_sddl("O:BUD:P(A;;FA;;;BA)").unwrap();
        assert!(foreign.admit(AdmissionPolicy::DatabaseSidecar).is_err());
        let readable = SecurityDescriptor::from_sddl("O:BAD:P(A;;FA;;;BA)(A;;GR;;;WD)").unwrap();
        assert!(readable.admit(AdmissionPolicy::DatabaseSidecar).is_err());
    }

    fn set_fixture_dacl(path: &std::path::Path, descriptor: &SecurityDescriptor) {
        use std::os::windows::ffi::OsStrExt;
        use windows_sys::Win32::Security::Authorization::SetNamedSecurityInfoW;
        use windows_sys::Win32::Security::PROTECTED_DACL_SECURITY_INFORMATION;

        let mut present = 0;
        let mut defaulted = 0;
        let mut dacl = ptr::null_mut();
        // SAFETY: descriptor owns a valid immutable descriptor. The three output
        // slots are distinct and writable; the resulting DACL borrows descriptor.
        assert_ne!(
            unsafe {
                GetSecurityDescriptorDacl(
                    descriptor.0.0.as_ptr(),
                    &mut present,
                    &mut dacl,
                    &mut defaulted,
                )
            },
            0
        );
        assert_ne!(present, 0);
        assert!(!dacl.is_null());
        let name: Vec<u16> = path.as_os_str().encode_wide().chain(Some(0)).collect();
        // SAFETY: name is an initialized NUL-terminated path kept live for the
        // call. dacl is the complete validated ACL borrowed from descriptor;
        // optional unrequested owner/group/SACL inputs are null. Win32 copies
        // the ACL, taking no ownership of either allocation. All callers pass
        // newly generated empty fixture directories, never a user store.
        let error = unsafe {
            SetNamedSecurityInfoW(
                name.as_ptr(),
                SE_FILE_OBJECT,
                DACL_SECURITY_INFORMATION | PROTECTED_DACL_SECURITY_INFORMATION,
                ptr::null_mut(),
                ptr::null_mut(),
                dacl,
                ptr::null(),
            )
        };
        assert_eq!(error, 0, "{}", io::Error::from_raw_os_error(error as i32));
    }

    struct RemoveFixture(std::path::PathBuf);

    impl Drop for RemoveFixture {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    #[test]
    fn database_parent_inheritance_is_checked_before_lease_creation_on_ntfs() {
        use crate::windows::Directory;
        use std::ffi::OsStr;

        let root_path =
            std::env::temp_dir().join(format!("mneme-private-children-{}", ulid::Ulid::new()));
        // Declared before handles so they close before fixture removal.
        let _cleanup = RemoveFixture(root_path.clone());
        let root = Directory::create(&root_path).unwrap();
        let user = TokenSid::user().unwrap().sid_string().unwrap();
        for (name, broad_ace) in [
            ("inherit-read", "(A;OIIO;GR;;;WD)"),
            ("inherit-write", "(A;OIIO;GW;;;WD)"),
            ("effective-read", "(A;OI;GR;;;WD)"),
        ] {
            let original = root.create_directory(OsStr::new(name)).unwrap();
            let path = original.path().to_path_buf();
            drop(original);
            let acl = SecurityDescriptor::from_sddl(&format!(
                "O:{user}D:P(A;OICI;FA;;;{user})(A;OICI;FA;;;SY)(A;OICI;FA;;;BA){broad_ace}"
            ))
            .unwrap();
            set_fixture_dacl(&path, &acl);
            let parent = Directory::open_parent(&path).unwrap();
            assert_eq!(
                parent.require_private_children().unwrap_err().kind(),
                io::ErrorKind::PermissionDenied
            );
            assert!(crate::StoreLease::acquire(&path.join("memory.db")).is_err());
            assert!(!path.join("memory.db.mneme.lock").exists());

            // Generic parents remain usable for protected Mneme child creation.
            let protected = parent.create_directory(OsStr::new("protected")).unwrap();
            protected.require_private_children().unwrap();
            let private_sidecar = File::create(protected.path().join("plain-sidecar")).unwrap();
            require_private_database_sidecar(&private_sidecar).unwrap();

            // Exercise the filesystem's actual default-descriptor inheritance,
            // independently of the SDDL parser: plain creation inherits the bad
            // grant whereas the explicit protected child above does not.
            let inherited = File::create(path.join("plain-sidecar")).unwrap();
            // Use the sidecar owner policy so an elevated BA default owner
            // cannot be mistaken for evidence that the inherited DACL is bad.
            assert!(require_private_database_sidecar(&inherited).is_err());
        }
    }

    #[test]
    fn database_parent_without_file_inheritance_is_refused_before_lease_on_ntfs() {
        use crate::windows::Directory;

        let path = std::env::temp_dir().join(format!("mneme-default-dacl-{}", ulid::Ulid::new()));
        let _cleanup = RemoveFixture(path.clone());
        let original = Directory::create(&path).unwrap();
        drop(original);
        // The file-style descriptor has trusted effective grants, but none are
        // inheritable. Default-security child creation must not reach an
        // unchecked token default DACL merely because this parent is private.
        let descriptor = PrivateSecurity::new(false).unwrap();
        set_fixture_dacl(&path, &descriptor.descriptor);
        let parent = Directory::open_parent(&path).unwrap();
        assert!(parent.require_private_children().is_err());
        assert!(crate::StoreLease::acquire(&path.join("memory.db")).is_err());
        assert!(!path.join("memory.db.mneme.lock").exists());
    }
}
