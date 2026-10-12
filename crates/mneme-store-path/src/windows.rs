//! Handle-relative Windows filesystem operations used by persistent stores.
//!
//! This is deliberately a local NTFS contract, not a generic Windows filesystem
//! abstraction. Reparse points (including junctions/cloud placeholders), remote
//! devices, alternate streams and multiply-linked regular files are refused.
//! Existing ACLs are checked, never silently repaired. New objects receive a
//! protected current-user/SYSTEM/Administrators ACL (administrators are trusted
//! in the same sense as root on Unix).
//!
//! Child operations use `NtCreateFile` relative to an already open directory.
//! Renames and deletion act on verified handles, not a second pathname lookup.
//! The ordinary Windows sharing flags permit retained handles across rename
//! and POSIX deletion; pathname guards detect detached/replaced parents.
//!
//! Native contracts: [NtCreateFile], [FILE_RENAME_INFORMATION],
//! [NtFlushBuffersFileEx] (flags zero: data, metadata and device-cache barrier),
//! and [FILE_ID_INFO]. Any unavailable filesystem primitive fails closed.
//!
//! [NtCreateFile]: https://learn.microsoft.com/en-us/windows/win32/api/winternl/nf-winternl-ntcreatefile
//! [FILE_RENAME_INFORMATION]: https://learn.microsoft.com/en-us/windows-hardware/drivers/ddi/ntifs/ns-ntifs-_file_rename_information
//! [NtFlushBuffersFileEx]: https://learn.microsoft.com/en-us/windows-hardware/drivers/ddi/ntifs/nf-ntifs-ntflushbuffersfileex
//! [FILE_ID_INFO]: https://learn.microsoft.com/en-us/windows/win32/api/winbase/ns-winbase-file_id_info

use std::ffi::{OsStr, OsString};
use std::fs::File;
use std::io;
use std::mem::{align_of, offset_of, size_of};
use std::os::windows::ffi::OsStrExt;
use std::os::windows::io::{AsRawHandle, FromRawHandle};
use std::path::{Component, Path, PathBuf, Prefix};
use std::ptr;

use windows_sys::Wdk::Foundation::OBJECT_ATTRIBUTES;
use windows_sys::Wdk::Storage::FileSystem::{
    FILE_CREATE, FILE_DIRECTORY_FILE, FILE_DISPOSITION_DELETE, FILE_DISPOSITION_INFORMATION_EX,
    FILE_DISPOSITION_POSIX_SEMANTICS, FILE_NON_DIRECTORY_FILE, FILE_OPEN, FILE_OPEN_REPARSE_POINT,
    FILE_RENAME_INFORMATION, FILE_SYNCHRONOUS_IO_NONALERT, FileDispositionInformationEx,
    FileFsDeviceInformation, FileRenameInformation, NtCreateFile, NtFlushBuffersFileEx,
    NtQueryVolumeInformationFile, NtSetInformationFile,
};
use windows_sys::Wdk::System::SystemServices::{FILE_FS_DEVICE_INFORMATION, FILE_REMOTE_DEVICE};
use windows_sys::Win32::Foundation::{
    HANDLE, OBJ_CASE_INSENSITIVE, RtlNtStatusToDosError, UNICODE_STRING,
};
use windows_sys::Win32::Storage::FileSystem::{
    DELETE, FILE_ADD_FILE, FILE_ADD_SUBDIRECTORY, FILE_ATTRIBUTE_DIRECTORY, FILE_ATTRIBUTE_NORMAL,
    FILE_ATTRIBUTE_OFFLINE, FILE_ATTRIBUTE_REPARSE_POINT, FILE_ATTRIBUTE_TAG_INFO, FILE_BASIC_INFO,
    FILE_DEVICE_DISK, FILE_ID_INFO, FILE_LIST_DIRECTORY, FILE_READ_ATTRIBUTES, FILE_READ_DATA,
    FILE_SHARE_DELETE, FILE_SHARE_READ, FILE_SHARE_WRITE, FILE_STANDARD_INFO, FILE_TRAVERSE,
    FILE_WRITE_ATTRIBUTES, FILE_WRITE_DATA, FileAttributeTagInfo, FileBasicInfo, FileIdInfo,
    FileStandardInfo, GetFileInformationByHandleEx, GetVolumeInformationByHandleW, READ_CONTROL,
    SYNCHRONIZE,
};
use windows_sys::Win32::System::IO::IO_STATUS_BLOCK;

mod activation;
mod security;
pub(crate) use activation::require_plain_existing_path;
pub use activation::{ActivationSelector, encode_activation_selector, read_activation_selector};

/// Full Windows file identity plus the state used for exact sealed-file checks.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct FileIdentity {
    pub volume: u64,
    pub id: [u8; 16],
    pub links: u32,
    pub len: u64,
}

/// Closed-source evidence: object identity and Windows modification timestamps.
/// Access time is intentionally excluded so a read does not invalidate evidence.
/// This is a checked sample, not a content lock or substitute for content hashes.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct FileRevision {
    pub identity: FileIdentity,
    pub creation_time: i64,
    pub write_time: i64,
    pub change_time: i64,
    pub attributes: u32,
}

impl FileIdentity {
    /// Compare objects, ignoring directory child counts and file length.
    pub fn same_object(self, other: Self) -> bool {
        self.volume == other.volume && self.id == other.id
    }

    /// Compare a retained regular file while its contents may grow.
    pub fn same_file(self, other: Self) -> bool {
        self.same_object(other) && self.links == other.links
    }
}

/// A verified current-owner local NTFS directory retained across child operations.
#[derive(Debug)]
pub struct Directory {
    file: File,
    path: PathBuf,
    identity: FileIdentity,
    private: bool,
}

impl Directory {
    /// Open an existing private directory. Does not create or change ACLs.
    pub fn open(path: &Path) -> io::Result<Self> {
        Self::open_checked(path, true)
    }

    /// Open an owned parent that may be readable, but not writable, by others.
    pub fn open_parent(path: &Path) -> io::Result<Self> {
        Self::open_checked(path, false)
    }

    fn open_checked(path: &Path, private: bool) -> io::Result<Self> {
        let path = absolute_disk_path(path)?;
        let file = open_absolute_directory(&path, true)?;
        require_local_ntfs(&file)?;
        let identity = directory_identity(&file)?;
        if private {
            security::require_private(&file, true)?;
        } else {
            security::require_safe_parent(&file)?;
        }
        Ok(Self {
            file,
            path,
            identity,
            private,
        })
    }

    /// Create one private directory, refusing an existing name. Not recursive.
    pub fn create(path: &Path) -> io::Result<Self> {
        let path = absolute_disk_path(path)?;
        let parent_path = path
            .parent()
            .ok_or_else(|| invalid("directory has no parent"))?;
        let parent = Self::open_parent(parent_path)?;
        let name = path
            .file_name()
            .ok_or_else(|| invalid("directory has no name"))?;
        parent.create_directory(name)
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Identity captured at open. Use `require_named` before relying on it later.
    pub fn identity(&self) -> FileIdentity {
        self.identity
    }

    /// Prove the named directory is still the retained object and remains safe.
    pub fn require_named(&self) -> io::Result<()> {
        let retained = directory_identity(&self.file)?;
        let named = open_absolute_directory(&self.path, false)?;
        if !retained.same_object(self.identity)
            || !directory_identity(&named)?.same_object(self.identity)
        {
            return Err(changed("Windows directory pathname changed identity"));
        }
        if self.private {
            security::require_private(&self.file, true)
        } else {
            security::require_safe_parent(&self.file)
        }
    }

    pub fn require_private(&self) -> io::Result<()> {
        self.require_named()?;
        security::require_private(&self.file, true)
    }

    /// Prove newly created ordinary files inherit a private ACL. SQLite's VFS
    /// creates WAL/SHM/journal files without our protected creation descriptor,
    /// so its actual database parent must pass this stronger check. Generic
    /// ancestors used only to create protected Mneme children need not pass it.
    pub fn require_private_children(&self) -> io::Result<()> {
        self.require_named()?;
        security::require_private_children(&self.file).map_err(|error| {
            io::Error::new(error.kind(), format!(
                "Windows database parent {} cannot create private SQLite sidecars: {error}; use a private Mneme-created directory (existing ACLs are not rewritten)",
                self.path.display()
            ))
        })
    }

    /// Flush directory metadata and the underlying storage cache; never a no-op.
    pub fn sync_all(&self) -> io::Result<()> {
        let mut status = IO_STATUS_BLOCK::default();
        // SAFETY: self.file owns a live synchronous NT file handle, opened with
        // directory write/append rights. status is initialized, writable and
        // exclusively borrowed for this synchronous call. Flags zero request a
        // full flush; the API requires null Parameters and ParametersSize zero.
        let result = unsafe {
            NtFlushBuffersFileEx(self.file.as_raw_handle(), 0, ptr::null(), 0, &mut status)
        };
        nt_result(result)
    }

    /// Open a no-follow private, single-link regular child, without creating it.
    pub fn open_file(&self, name: &OsStr, writable: bool) -> io::Result<File> {
        self.open_regular_child(name, writable, writable)
    }

    /// Open a checked existing database handle without DELETE access, compatible
    /// with SQLite's Windows main-file sharing policy even while writable.
    pub fn open_database_file(&self, name: &OsStr, writable: bool) -> io::Result<File> {
        self.require_private_children()?;
        self.open_regular_child(name, writable, false)
    }

    fn open_regular_child(&self, name: &OsStr, writable: bool, delete: bool) -> io::Result<File> {
        let mut access = FILE_READ_DATA | FILE_READ_ATTRIBUTES | READ_CONTROL | SYNCHRONIZE;
        if writable {
            access |= FILE_WRITE_DATA | FILE_WRITE_ATTRIBUTES;
        }
        if delete {
            access |= DELETE;
        }
        let file = open_child(&self.file, name, access, FILE_OPEN, false, None)?;
        file_identity(&file)?;
        require_private_file(&file)?;
        Ok(file)
    }

    /// Create an empty protected regular child, refusing any existing name.
    pub fn create_file(&self, name: &OsStr) -> io::Result<File> {
        self.create_file_with_access(name, true)
    }

    /// Create a retained database main handle compatible with SQLite's Windows
    /// VFS, which does not grant FILE_SHARE_DELETE to already-open handles.
    /// Publication acquires a separate DELETE handle only after SQLite closes.
    pub fn create_database_file(&self, name: &OsStr) -> io::Result<File> {
        self.require_private_children()?;
        self.create_file_with_access(name, false)
    }

    fn create_file_with_access(&self, name: &OsStr, delete: bool) -> io::Result<File> {
        self.require_named()?;
        let security = security::PrivateSecurity::new(false)?;
        let file = open_child(
            &self.file,
            name,
            FILE_READ_DATA
                | FILE_WRITE_DATA
                | FILE_WRITE_ATTRIBUTES
                | FILE_READ_ATTRIBUTES
                | READ_CONTROL
                | SYNCHRONIZE
                | if delete { DELETE } else { 0 },
            FILE_CREATE,
            false,
            Some(&security),
        )?;
        file_identity(&file)?;
        require_private_file(&file)?;
        Ok(file)
    }

    pub fn open_directory(&self, name: &OsStr) -> io::Result<Self> {
        let file = open_child(
            &self.file,
            name,
            directory_access(true),
            FILE_OPEN,
            true,
            None,
        )?;
        let identity = directory_identity(&file)?;
        security::require_private(&file, true)?;
        if identity.volume != self.identity.volume {
            return Err(invalid("child directory moved to another volume"));
        }
        Ok(Self {
            file,
            path: self.path.join(name),
            identity,
            private: true,
        })
    }

    pub fn create_directory(&self, name: &OsStr) -> io::Result<Self> {
        self.require_named()?;
        let security = security::PrivateSecurity::new(true)?;
        let file = open_child(
            &self.file,
            name,
            directory_access(true),
            FILE_CREATE,
            true,
            Some(&security),
        )?;
        let identity = directory_identity(&file)?;
        security::require_private(&file, true)?;
        let created = Self {
            file,
            path: self.path.join(name),
            identity,
            private: true,
        };
        created.sync_all()?;
        self.sync_all()?;
        created.require_named()?;
        Ok(created)
    }

    /// Delete exactly the named private regular object, never an unchecked name.
    /// The retained file can remain open; POSIX disposition removes the name.
    pub fn remove_file(&self, name: &OsStr, expected: FileIdentity) -> io::Result<()> {
        self.require_named()?;
        let file = self.open_file(name, true)?;
        if file_identity(&file)? != expected {
            return Err(changed("refusing to delete changed Windows file"));
        }
        delete_handle(&file)
    }

    /// Delete an exact empty child directory. Never recursively removes entries.
    pub fn remove_directory(&self, name: &OsStr, expected: FileIdentity) -> io::Result<()> {
        self.require_named()?;
        let file = open_child(
            &self.file,
            name,
            directory_access(true) | DELETE,
            FILE_OPEN,
            true,
            None,
        )?;
        security::require_private(&file, true)?;
        if !directory_identity(&file)?.same_object(expected) {
            return Err(changed("refusing to delete changed Windows directory"));
        }
        delete_handle(&file)
    }
}

/// Validate and inspect a regular non-reparse single-link file through its handle.
pub fn file_identity(file: &File) -> io::Result<FileIdentity> {
    let (identity, attributes, directory) = inspect(file)?;
    if directory || attributes & FILE_ATTRIBUTE_DIRECTORY != 0 || identity.links != 1 {
        return Err(invalid(
            "Windows store file must be regular with exactly one link",
        ));
    }
    Ok(identity)
}

pub fn require_private_file(file: &File) -> io::Result<()> {
    file_identity(file)?;
    security::require_private(file, false)
}

/// Sample an existing regular file's full identity and Windows revision fields.
/// The caller must retain the handle and compare another sample after reading.
pub fn file_revision(file: &File) -> io::Result<FileRevision> {
    let identity = file_identity(file)?;
    let mut basic = FILE_BASIC_INFO::default();
    // SAFETY: file is a live borrowed handle. basic is an initialized writable
    // SDK repr(C) FILE_BASIC_INFO, and its exact byte size matches FileBasicInfo.
    // The synchronous query retains neither pointer nor handle ownership.
    if unsafe {
        GetFileInformationByHandleEx(
            file.as_raw_handle(),
            FileBasicInfo,
            ptr::from_mut(&mut basic).cast(),
            size_of::<FILE_BASIC_INFO>() as u32,
        )
    } == 0
    {
        return Err(io::Error::last_os_error());
    }
    if file_identity(file)? != identity {
        return Err(changed(
            "Windows file changed while sampling revision evidence",
        ));
    }
    Ok(FileRevision {
        identity,
        creation_time: basic.CreationTime,
        write_time: basic.LastWriteTime,
        change_time: basic.ChangeTime,
        attributes: basic.FileAttributes,
    })
}

/// Read an existing local-NTFS regular file through no-follow parent traversal.
/// `private` additionally disallows other users' read grants; otherwise source
/// artifacts may be readable by others but must be current-owner and not writable
/// by them. This does not grant directory mutation authority or create anything.
pub fn open_regular_checked(path: &Path, private: bool) -> io::Result<File> {
    let file = open_regular_read(path)?;
    if private {
        security::require_private(&file, false)?;
    } else {
        security::require_safe_parent(&file)?;
    }
    Ok(file)
}

/// Read a private SQLite sidecar through no-follow local-NTFS traversal.
/// SQLite uses the effective token's default owner rather than Mneme's explicit
/// creation descriptor. A sidecar may therefore be owned by the current user or
/// Builtin Administrators, which are already trusted by the private DACL policy.
/// This stable owner set permits reopening an elevated-created WAL/SHM file
/// unelevated. It does not widen main-file, body, or generic file admission.
pub fn open_database_sidecar_checked(path: &Path) -> io::Result<File> {
    let file = open_regular_read(path)?;
    security::require_private_database_sidecar(&file)?;
    Ok(file)
}

fn open_regular_read(path: &Path) -> io::Result<File> {
    let path = absolute_disk_path(path)?;
    let parent_path = path
        .parent()
        .ok_or_else(|| invalid("regular file has no parent"))?;
    let parent = open_absolute_directory(parent_path, false)?;
    let name = path
        .file_name()
        .ok_or_else(|| invalid("regular file has no leaf"))?;
    let file = open_child(
        &parent,
        name,
        FILE_READ_DATA | FILE_READ_ATTRIBUTES | READ_CONTROL | SYNCHRONIZE,
        FILE_OPEN,
        false,
        None,
    )?;
    file_identity(&file)?;
    Ok(file)
}

/// Rename the retained source into an absent child of `parent`, on the same volume.
/// Source must have been opened with DELETE access. Does not replace or copy.
pub fn rename_no_replace(file: &File, parent: &Directory, name: &OsStr) -> io::Result<()> {
    let name = component_wide(name)?;
    parent.require_named()?;
    require_private_file(file)?;
    if file_identity(file)?.volume != parent.identity.volume {
        return Err(invalid("Windows publication must stay on one volume"));
    }
    rename_handle_no_replace(file, parent, &name)
}

/// Rename an exact private directory into an absent sibling namespace. The
/// retained source path is not rebound: after crossing, callers must validate
/// the target identity and establish their own typed recovery/durability cut.
/// No fallible post-rename operation is hidden behind an undifferentiated error.
pub fn rename_directory_no_replace(
    source: &Directory,
    parent: &Directory,
    name: &OsStr,
) -> io::Result<()> {
    let name = component_wide(name)?;
    source.require_private()?;
    parent.require_named()?;
    if source.identity.volume != parent.identity.volume {
        return Err(invalid(
            "directory publication must stay on one Windows volume",
        ));
    }
    let source_parent_path = source
        .path
        .parent()
        .ok_or_else(|| invalid("cannot rename volume root"))?;
    let source_parent = open_absolute_directory(source_parent_path, false)?;
    let source_name = source
        .path
        .file_name()
        .ok_or_else(|| invalid("directory has no leaf name"))?;
    let file = open_child(
        &source_parent,
        source_name,
        directory_access(true) | DELETE,
        FILE_OPEN,
        true,
        None,
    )?;
    security::require_private(&file, true)?;
    if !directory_identity(&file)?.same_object(source.identity) {
        return Err(changed("directory publication source changed identity"));
    }
    rename_handle_no_replace(&file, parent, &name)
}

fn rename_handle_no_replace(file: &File, parent: &Directory, name: &[u16]) -> io::Result<()> {
    let offset = offset_of!(FILE_RENAME_INFORMATION, FileName);
    let bytes = offset
        .checked_add(std::mem::size_of_val(name))
        .ok_or_else(|| invalid("rename buffer exceeds address space"))?;
    // A usize-backed allocation has sufficient alignment for all pointer/u32/u16
    // fields of the SDK's repr(C) FILE_RENAME_INFORMATION, checked explicitly.
    assert!(align_of::<usize>() >= align_of::<FILE_RENAME_INFORMATION>());
    let allocation = bytes
        .max(size_of::<FILE_RENAME_INFORMATION>())
        .div_ceil(size_of::<usize>());
    let mut buffer = vec![0usize; allocation];
    let information = buffer.as_mut_ptr().cast::<FILE_RENAME_INFORMATION>();
    let length = u32::try_from(bytes).map_err(|_| invalid("rename buffer is too large"))?;
    let mut status = IO_STATUS_BLOCK::default();
    // SAFETY: buffer is zero initialized and aligned for the SDK repr(C) type;
    // its size includes the complete header and name bytes. The raw writes only
    // initialize actual fields; no reference spans flexible-array trailing data.
    // `name` is initialized UTF-16 and nonoverlapping with buffer. Both buffers,
    // the borrowed live source/parent handles, and writable status outlive the
    // synchronous NtSetInformationFile call. False ReplaceIfExists is the
    // documented no-clobber operation; RootDirectory resolves one validated leaf.
    let result = unsafe {
        ptr::addr_of_mut!((*information).Anonymous.ReplaceIfExists).write(false);
        ptr::addr_of_mut!((*information).RootDirectory).write(parent.file.as_raw_handle());
        ptr::addr_of_mut!((*information).FileNameLength).write((name.len() * 2) as u32);
        ptr::copy_nonoverlapping(
            name.as_ptr(),
            buffer.as_mut_ptr().cast::<u8>().add(offset).cast::<u16>(),
            name.len(),
        );
        NtSetInformationFile(
            file.as_raw_handle(),
            &mut status,
            information.cast(),
            length,
            FileRenameInformation,
        )
    };
    nt_result(result)
}

fn delete_handle(file: &File) -> io::Result<()> {
    let information = FILE_DISPOSITION_INFORMATION_EX {
        Flags: FILE_DISPOSITION_DELETE | FILE_DISPOSITION_POSIX_SEMANTICS,
    };
    let mut status = IO_STATUS_BLOCK::default();
    // SAFETY: file owns the live DELETE-capable handle. information is a fully
    // initialized SDK repr(C) structure whose exact size is supplied; status is
    // initialized and writable. Neither pointer is retained after this synchronous
    // call. No fallback to name-based deletion follows a refusal.
    let result = unsafe {
        NtSetInformationFile(
            file.as_raw_handle(),
            &mut status,
            ptr::from_ref(&information).cast(),
            size_of::<FILE_DISPOSITION_INFORMATION_EX>() as u32,
            FileDispositionInformationEx,
        )
    };
    nt_result(result)
}

fn directory_identity(file: &File) -> io::Result<FileIdentity> {
    let (identity, attributes, directory) = inspect(file)?;
    if !directory || attributes & FILE_ATTRIBUTE_DIRECTORY == 0 {
        return Err(invalid("Windows store parent must be a directory"));
    }
    Ok(identity)
}

fn inspect(file: &File) -> io::Result<(FileIdentity, u32, bool)> {
    let mut id = FILE_ID_INFO::default();
    let mut standard = FILE_STANDARD_INFO::default();
    let mut attributes = FILE_ATTRIBUTE_TAG_INFO::default();
    // SAFETY: each output points to a writable initialized SDK repr(C) structure
    // matching the requested information class, with the exact byte size. file's
    // live borrowed handle outlives all synchronous queries; no pointer is retained.
    unsafe {
        if GetFileInformationByHandleEx(
            file.as_raw_handle(),
            FileIdInfo,
            ptr::from_mut(&mut id).cast(),
            size_of::<FILE_ID_INFO>() as u32,
        ) == 0
            || GetFileInformationByHandleEx(
                file.as_raw_handle(),
                FileStandardInfo,
                ptr::from_mut(&mut standard).cast(),
                size_of::<FILE_STANDARD_INFO>() as u32,
            ) == 0
            || GetFileInformationByHandleEx(
                file.as_raw_handle(),
                FileAttributeTagInfo,
                ptr::from_mut(&mut attributes).cast(),
                size_of::<FILE_ATTRIBUTE_TAG_INFO>() as u32,
            ) == 0
        {
            return Err(io::Error::last_os_error());
        }
    }
    if attributes.FileAttributes & (FILE_ATTRIBUTE_REPARSE_POINT | FILE_ATTRIBUTE_OFFLINE) != 0 {
        return Err(invalid(
            "reparse points and offline files are not Windows store objects",
        ));
    }
    if standard.DeletePending {
        return Err(changed("Windows store object is pending deletion"));
    }
    let len =
        u64::try_from(standard.EndOfFile).map_err(|_| invalid("negative Windows file length"))?;
    Ok((
        FileIdentity {
            volume: id.VolumeSerialNumber,
            id: id.FileId.Identifier,
            links: standard.NumberOfLinks,
            len,
        },
        attributes.FileAttributes,
        standard.Directory,
    ))
}

fn require_local_ntfs(file: &File) -> io::Result<()> {
    let mut device = FILE_FS_DEVICE_INFORMATION::default();
    let mut status = IO_STATUS_BLOCK::default();
    let mut fs_name = [0u16; 32];
    // SAFETY: file is a live synchronous NT handle. device/status are initialized
    // writable SDK structures with exact sizes. fs_name is a 32-u16 writable
    // buffer and the remaining optional outputs are null with zero lengths.
    // Both APIs complete before these borrowed buffers/handle cease to be valid.
    unsafe {
        nt_result(NtQueryVolumeInformationFile(
            file.as_raw_handle(),
            &mut status,
            ptr::from_mut(&mut device).cast(),
            size_of::<FILE_FS_DEVICE_INFORMATION>() as u32,
            FileFsDeviceInformation,
        ))?;
        if GetVolumeInformationByHandleW(
            file.as_raw_handle(),
            ptr::null_mut(),
            0,
            ptr::null_mut(),
            ptr::null_mut(),
            ptr::null_mut(),
            fs_name.as_mut_ptr(),
            fs_name.len() as u32,
        ) == 0
        {
            return Err(io::Error::last_os_error());
        }
    }
    let end = fs_name
        .iter()
        .position(|unit| *unit == 0)
        .unwrap_or(fs_name.len());
    if device.DeviceType != FILE_DEVICE_DISK
        || device.Characteristics & FILE_REMOTE_DEVICE != 0
        || fs_name[..end] != [b'N' as u16, b'T' as u16, b'F' as u16, b'S' as u16]
    {
        return Err(io::Error::new(
            io::ErrorKind::Unsupported,
            "Windows persistent stores require a local NTFS volume (network/ReFS/FAT are not qualified)",
        ));
    }
    Ok(())
}

fn directory_access(writable: bool) -> u32 {
    let access =
        FILE_LIST_DIRECTORY | FILE_TRAVERSE | FILE_READ_ATTRIBUTES | READ_CONTROL | SYNCHRONIZE;
    if writable {
        access | FILE_ADD_FILE | FILE_ADD_SUBDIRECTORY
    } else {
        access
    }
}

fn open_child(
    parent: &File,
    name: &OsStr,
    access: u32,
    disposition: u32,
    directory: bool,
    security: Option<&security::PrivateSecurity>,
) -> io::Result<File> {
    let name = component_wide(name)?;
    // Existing directory opens intentionally omit FILE_DIRECTORY_FILE: the NT
    // user-mode docs enumerate a narrower compatible flag list for that flag.
    // Leaf no-follow is always requested and type is checked through the handle.
    let options = if disposition == FILE_CREATE && directory {
        FILE_DIRECTORY_FILE
    } else if !directory {
        FILE_NON_DIRECTORY_FILE | FILE_OPEN_REPARSE_POINT
    } else {
        FILE_OPEN_REPARSE_POINT
    };
    open_nt(Some(parent), name, access, disposition, options, security)
}

fn open_nt(
    parent: Option<&File>,
    mut name: Vec<u16>,
    access: u32,
    disposition: u32,
    options: u32,
    security: Option<&security::PrivateSecurity>,
) -> io::Result<File> {
    let len = u16::try_from(name.len() * 2).map_err(|_| invalid("Windows name is too long"))?;
    let unicode = UNICODE_STRING {
        Length: len,
        MaximumLength: len,
        Buffer: name.as_mut_ptr(),
    };
    let attributes = OBJECT_ATTRIBUTES {
        Length: size_of::<OBJECT_ATTRIBUTES>() as u32,
        RootDirectory: parent.map_or(ptr::null_mut(), AsRawHandle::as_raw_handle),
        ObjectName: &unicode,
        Attributes: OBJ_CASE_INSENSITIVE,
        SecurityDescriptor: security.map_or(ptr::null(), |security| security.as_ptr().cast()),
        SecurityQualityOfService: ptr::null(),
    };
    let mut handle: HANDLE = ptr::null_mut();
    let mut status = IO_STATUS_BLOCK::default();
    // SAFETY: unicode describes exactly the initialized UTF-16 allocation, whose
    // byte length fits u16; it need not be NUL terminated. All structures and the
    // optional owned security descriptor outlive this synchronous call. A parent
    // is borrowed from a live File. Out parameters are initialized/writable and
    // disjoint. No handle inheritance or asynchronous IO is requested.
    let result = unsafe {
        NtCreateFile(
            &mut handle,
            access,
            &attributes,
            &mut status,
            ptr::null(),
            FILE_ATTRIBUTE_NORMAL,
            FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE,
            disposition,
            options | FILE_SYNCHRONOUS_IO_NONALERT,
            ptr::null(),
            0,
        )
    };
    nt_result(result)?;
    // SAFETY: successful NtCreateFile returned a new owned file handle; this is
    // its sole ownership transfer and File closes it exactly once. The requested
    // synchronous IO mode is compatible with std::fs::File's Read/Write methods.
    Ok(unsafe { File::from_raw_handle(handle) })
}

fn open_absolute_directory(path: &Path, writable: bool) -> io::Result<File> {
    let (drive, components) = disk_components(path)?;
    let root = format!("\\??\\{}:\\", char::from(drive));
    let mut file = open_nt(
        None,
        root.encode_utf16().collect(),
        directory_access(false),
        FILE_OPEN,
        FILE_OPEN_REPARSE_POINT,
        None,
    )?;
    directory_identity(&file)?;
    require_local_ntfs(&file)?;
    let count = components.len();
    for (index, component) in components.into_iter().enumerate() {
        file = open_child(
            &file,
            &component,
            directory_access(writable && index + 1 == count),
            FILE_OPEN,
            true,
            None,
        )?;
        directory_identity(&file)?;
    }
    Ok(file)
}

fn absolute_disk_path(path: &Path) -> io::Result<PathBuf> {
    let path = if path == Path::new(".") {
        std::env::current_dir()?
    } else if path.is_absolute() {
        path.to_path_buf()
    } else {
        // Drive-relative and root-relative spellings must not depend on hidden
        // per-drive process state. Only an ordinary relative path is anchored.
        if path
            .components()
            .any(|c| matches!(c, Component::Prefix(_) | Component::RootDir))
        {
            return Err(invalid(
                "Windows store path must be drive-absolute or ordinary relative",
            ));
        }
        let raw: Vec<u16> = path.as_os_str().encode_wide().collect();
        use std::os::windows::ffi::OsStringExt;
        for component in raw.split(|unit| *unit == 92 || *unit == 47) {
            component_wide(&OsString::from_wide(component))?;
        }
        std::env::current_dir()?.join(path)
    };
    validate_absolute_path(&path)?;
    let (drive, components) = disk_components(&path)?;
    let mut normalized = PathBuf::from(format!("\\\\?\\{}:\\", char::from(drive)));
    for component in components {
        normalized.push(component);
    }
    Ok(normalized)
}

fn disk_components(path: &Path) -> io::Result<(u8, Vec<OsString>)> {
    let mut parts = path.components();
    let drive = match parts.next() {
        Some(Component::Prefix(prefix)) => match prefix.kind() {
            Prefix::Disk(drive) | Prefix::VerbatimDisk(drive) => drive,
            _ => {
                return Err(invalid(
                    "Windows stores require a local drive path, not UNC/device paths",
                ));
            }
        },
        _ => return Err(invalid("Windows store path has no drive")),
    };
    if !matches!(parts.next(), Some(Component::RootDir)) {
        return Err(invalid(
            "drive-relative Windows store paths are not supported",
        ));
    }
    let mut names = Vec::new();
    for part in parts {
        match part {
            Component::Normal(name) => {
                component_wide(name)?;
                names.push(name.to_os_string());
            }
            _ => {
                return Err(invalid(
                    "Windows store path contains a non-normal component",
                ));
            }
        }
    }
    Ok((drive, names))
}

/// Validate a literal absolute local-drive spelling before Path component
/// normalization could erase dot, empty or stream components.
pub fn validate_absolute_path(path: &Path) -> io::Result<()> {
    let wide: Vec<u16> = path.as_os_str().encode_wide().collect();
    let prefix: &[u16] = &[92, 92, 63, 92];
    let start = if wide.starts_with(prefix) { 4 } else { 0 };
    let drive = wide.get(start).copied().unwrap_or(0);
    let separator = |unit: u16| unit == 92 || (start == 0 && unit == 47);
    if !((u16::from(b'A')..=u16::from(b'Z')).contains(&drive)
        || (u16::from(b'a')..=u16::from(b'z')).contains(&drive))
        || wide.get(start + 1) != Some(&58)
        || !wide.get(start + 2).copied().is_some_and(separator)
    {
        return Err(invalid(
            "Windows path must have a local drive and absolute root",
        ));
    }
    let tail = &wide[start + 3..];
    if !tail.is_empty() {
        use std::os::windows::ffi::OsStringExt;
        for segment in tail.split(|unit| separator(*unit)) {
            component_wide(&OsString::from_wide(segment))?;
        }
    }
    Ok(())
}

/// Validate one ordinary Windows child name, excluding ADS and DOS devices.
pub fn validate_leaf(name: &OsStr) -> io::Result<()> {
    component_wide(name).map(|_| ())
}

fn component_wide(name: &OsStr) -> io::Result<Vec<u16>> {
    let wide: Vec<u16> = name.encode_wide().collect();
    if wide.is_empty()
        || wide.len() > 255
        || name == OsStr::new(".")
        || name == OsStr::new("..")
        || matches!(wide.last(), Some(&32) | Some(&46))
        || wide
            .iter()
            .any(|unit| *unit < 32 || b"/\\:<>\"|?*".iter().any(|bad| *unit == u16::from(*bad)))
    {
        return Err(invalid(
            "Windows store child name is not one unambiguous normal component",
        ));
    }
    let stem = wide.split(|unit| *unit == 46).next().unwrap_or(&[]);
    let stem: Vec<u16> = stem
        .iter()
        .copied()
        .map(|unit| {
            if (97..=122).contains(&unit) {
                unit - 32
            } else {
                unit
            }
        })
        .collect();
    let ascii = |name: &str| name.encode_utf16().collect::<Vec<_>>();
    if ["CON", "PRN", "AUX", "NUL"]
        .iter()
        .any(|name| stem == ascii(name))
        || (stem.len() == 4
            && (stem[..3] == ascii("COM") || stem[..3] == ascii("LPT"))
            && ((49..=57).contains(&stem[3]) || [0xb9, 0xb2, 0xb3].contains(&stem[3])))
    {
        return Err(invalid("DOS device names are not Windows store filenames"));
    }
    Ok(wide)
}

fn nt_result(status: i32) -> io::Result<()> {
    if status >= 0 {
        return Ok(());
    }
    // SAFETY: RtlNtStatusToDosError accepts an arbitrary NTSTATUS scalar, writes
    // no caller memory and returns its corresponding Win32 error code.
    let error = unsafe { RtlNtStatusToDosError(status) };
    Err(io::Error::from_raw_os_error(error as i32))
}

fn invalid(message: &str) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, message)
}
fn changed(message: &str) -> io::Error {
    io::Error::new(io::ErrorKind::PermissionDenied, message)
}

pub(crate) fn require_database_identity(database: &Path) -> io::Result<()> {
    let database = absolute_disk_path(database)?;
    let parent = database
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
        .unwrap_or_else(|| Path::new("."));
    let parent = Directory::open_parent(parent)?;
    let name = database
        .file_name()
        .ok_or_else(|| invalid("database has no filename"))?;
    match parent.open_file(name, false) {
        Ok(_) => Ok(()),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(error),
    }
}

pub(crate) fn acquire_lease(database: &Path, lock_path: PathBuf) -> io::Result<crate::StoreLease> {
    let parent_path = lock_path
        .parent()
        .ok_or_else(|| invalid("lease has no parent"))?
        .to_path_buf();
    let parent = Directory::open_parent(&parent_path)?;
    parent.require_private_children()?;
    let name = lock_path
        .file_name()
        .ok_or_else(|| invalid("lease has no filename"))?;
    let file = match parent.create_file(name) {
        Ok(file) => {
            file.sync_all()?;
            parent.sync_all()?;
            file
        }
        Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {
            parent.open_file(name, true)?
        }
        Err(error) => return Err(error),
    };
    match file.try_lock() {
        Ok(()) => {}
        Err(std::fs::TryLockError::WouldBlock) => {
            return Err(io::Error::new(
                io::ErrorKind::WouldBlock,
                format!(
                    "database {} is already owned by another mneme process (lease {})",
                    database.display(),
                    lock_path.display()
                ),
            ));
        }
        Err(std::fs::TryLockError::Error(error)) => return Err(error),
    }
    let parent_identity = parent.identity();
    let lock_identity = file_identity(&file)?;
    let lease = crate::StoreLease {
        file,
        parent,
        lock_path,
        parent_path,
        parent_identity,
        lock_identity,
    };
    require_lease_guards(&lease)?;
    Ok(lease)
}

pub(crate) fn require_lease_guards(lease: &crate::StoreLease) -> io::Result<()> {
    lease.parent.require_private_children()?;
    if !lease.parent.identity().same_object(lease.parent_identity) {
        return Err(changed("Windows lease parent changed identity"));
    }
    require_private_file(&lease.file)?;
    let name = lease
        .lock_path
        .file_name()
        .ok_or_else(|| invalid("lease has no filename"))?;
    let named = lease.parent.open_file(name, false)?;
    if !file_identity(&named)?.same_file(lease.lock_identity)
        || !file_identity(&lease.file)?.same_file(lease.lock_identity)
    {
        return Err(changed("Windows lease pathname changed identity"));
    }
    Ok(())
}

#[cfg(test)]
mod tests;
