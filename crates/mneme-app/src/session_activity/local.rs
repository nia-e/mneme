//! Concrete local adapter. Unix uses component-by-component no-follow opens and
//! retains the admitted root descriptor. Other platforms fail closed for v1.

use super::*;
use std::fs::File;
use std::io::{self, Read};

/// A scope/configuration snapshot for a long-lived local-only frontend. Manifest
/// replacement cannot widen its workspace or retarget the held directory.
pub struct PreparedActivity {
    initial: ActivityReport,
    root: Option<Directory>,
}

impl PreparedActivity {
    pub fn new(manifest: &Path, cwd: &Path) -> Result<Self, ActivityError> {
        let mut initial = ActivityReport {
            schema: REPORT_SCHEMA,
            status: ActivityStatus::NotConfigured,
            scope: None,
            sessions: Vec::new(),
            coverage: Coverage::default(),
        };
        if !manifest.is_absolute() {
            return Err(ActivityError {
                code: "invalid_manifest_path",
                message: "The activity manifest path must be absolute.",
            });
        }
        let bytes = match read_absolute(manifest, MAX_MANIFEST_BYTES) {
            Ok(bytes) => bytes,
            Err(issue) => {
                initial.status = if issue == CoverageIssue::Missing {
                    ActivityStatus::NotConfigured
                } else {
                    ActivityStatus::Unavailable
                };
                initial.coverage.issue(issue);
                return Ok(Self {
                    initial,
                    root: None,
                });
            }
        };
        initial.coverage.read_bytes = bytes.len();
        initial.coverage.read_budget_used = MAX_MANIFEST_BYTES + 1;
        let manifest: Manifest = match decode_json(&bytes, MAX_MANIFEST_BYTES, MANIFEST_SCHEMA)
            .and_then(|_| serde_json::from_slice(&bytes).map_err(|_| CoverageIssue::Malformed))
        {
            Ok(manifest) => manifest,
            Err(issue) => {
                initial.status = ActivityStatus::InvalidManifest;
                initial.coverage.issue(issue);
                return Ok(Self {
                    initial,
                    root: None,
                });
            }
        };
        if !valid_manifest(&manifest) {
            initial.status = ActivityStatus::InvalidManifest;
            initial.coverage.issue(CoverageIssue::Malformed);
            return Ok(Self {
                initial,
                root: None,
            });
        }
        let cwd = match std::fs::canonicalize(cwd) {
            Ok(cwd) if cwd.is_dir() => cwd,
            _ => {
                initial.status = ActivityStatus::WorkspaceUnbound;
                initial.coverage.issue(CoverageIssue::Unreadable);
                return Ok(Self {
                    initial,
                    root: None,
                });
            }
        };
        let binding = manifest
            .workspaces
            .iter()
            .filter(|binding| cwd.starts_with(&binding.path))
            .max_by_key(|binding| binding.path.components().count());
        let Some(binding) = binding else {
            initial.status = ActivityStatus::WorkspaceUnbound;
            return Ok(Self {
                initial,
                root: None,
            });
        };
        // Unrelated stale worktrees are retained configuration, not a reason to
        // disable this scope. Only the selected binding gets filesystem checks;
        // if it fails, never retry a broader parent binding.
        if !std::fs::canonicalize(&binding.path)
            .is_ok_and(|path| path.as_os_str() == binding.path.as_os_str() && path.is_dir())
        {
            initial.status = ActivityStatus::WorkspaceUnbound;
            initial.coverage.issue(CoverageIssue::ScopeMismatch);
            return Ok(Self {
                initial,
                root: None,
            });
        }
        initial.scope = Some(ActivityScope {
            workspace: binding.path.to_string_lossy().into_owned(),
            source: binding.source.clone(),
        });
        let mut at = cwd.as_path();
        while at != binding.path {
            // Merely inspect marker presence. A worktree's .git file and a
            // symlinked marker are boundaries too; their contents are not read.
            for marker in [".git", ".mneme"] {
                match std::fs::symlink_metadata(at.join(marker)) {
                    Ok(_) => {
                        initial.status = ActivityStatus::WorkspaceBoundary;
                        return Ok(Self {
                            initial,
                            root: None,
                        });
                    }
                    Err(error) if error.kind() == io::ErrorKind::NotFound => {}
                    Err(_) => {
                        initial.status = ActivityStatus::WorkspaceBoundary;
                        initial.coverage.issue(CoverageIssue::Unreadable);
                        return Ok(Self {
                            initial,
                            root: None,
                        });
                    }
                }
            }
            let Some(parent) = at.parent() else {
                break;
            };
            at = parent;
        }
        match Directory::open(&manifest.root) {
            Ok(root) => {
                initial.status = ActivityStatus::Ready;
                Ok(Self {
                    initial,
                    root: Some(root),
                })
            }
            Err(issue) => {
                initial.status = if issue == CoverageIssue::Missing {
                    ActivityStatus::NoObservations
                } else {
                    ActivityStatus::Unavailable
                };
                initial.coverage.issue(issue);
                Ok(Self {
                    initial,
                    root: None,
                })
            }
        }
    }

    pub fn report(&self, request: ActivityRequest) -> Result<ActivityReport, ActivityError> {
        self.report_with_cancel(request, &AtomicBool::new(false))
    }

    pub fn report_with_cancel(
        &self,
        request: ActivityRequest,
        cancel: &AtomicBool,
    ) -> Result<ActivityReport, ActivityError> {
        let selector = parse_selector(request.session.as_deref())?;
        let mut report = self.initial.clone();
        if cancelled(cancel) {
            report.status = ActivityStatus::Cancelled;
            report.coverage.issue(CoverageIssue::Cancelled);
            return Ok(report);
        }
        let Some(root) = &self.root else {
            return Ok(report);
        };
        let scope = report
            .scope
            .as_ref()
            .expect("root exists only after workspace admission");
        let source = scope.source.clone();
        let workspace = scope.workspace.clone();
        let selected_id = match selector {
            Some((Some(selector_source), _)) if selector_source != source => {
                report.status = ActivityStatus::SessionNotFound;
                return Ok(report);
            }
            Some((_, id)) => Some(id),
            None => None,
        };
        if let Some(id) = selected_id {
            // A full stored ID wins before any catalog enumeration. Prefixes
            // simply miss this hashed path and fall through to bounded scanning.
            let name = record_filename(&source, id);
            report.coverage.scan = ScanKind::Direct;
            match read_record(
                root,
                &name,
                &source,
                &workspace,
                Some(id),
                &mut report.coverage,
            ) {
                Ok(record) => {
                    report.sessions.push(project(record, request.details));
                    bound_output(&mut report, cancel);
                    return Ok(report);
                }
                Err(CoverageIssue::Missing) => {}
                Err(issue) => {
                    report.coverage.issue(issue);
                    report.status = ActivityStatus::SessionUnavailable;
                    return Ok(report);
                }
            }
        }
        report.coverage.scan = ScanKind::Catalog;
        let mut scan = root.catalog(cancel);
        report.coverage.inspected_entries = scan.inspected;
        for issue in scan.issues {
            report.coverage.issue(issue);
        }
        // mtime is a search-order hint only, never an observation timestamp.
        scan.candidates.sort_by(|a, b| {
            b.modified
                .cmp(&a.modified)
                .then_with(|| a.name.cmp(&b.name))
        });
        let mut records = Vec::new();
        let mut complete = scan.complete;
        for candidate in scan.candidates {
            if cancelled(cancel) {
                report.coverage.issue(CoverageIssue::Cancelled);
                complete = false;
                break;
            }
            if report
                .coverage
                .read_budget_used
                .saturating_add(MAX_RECORD_BYTES + 1)
                > MAX_READ_BYTES
            {
                report.coverage.issue(CoverageIssue::ReadLimit);
                complete = false;
                break;
            }
            match read_record(
                root,
                &candidate.name,
                &source,
                &workspace,
                None,
                &mut report.coverage,
            ) {
                Ok(record) => records.push(record),
                Err(issue) => {
                    // A valid other-source/workspace record is outside this
                    // admitted scope, but corrupt/unreadable candidates prevent
                    // proof that a prefix is unique in it.
                    if issue != CoverageIssue::ScopeMismatch {
                        complete = false;
                    }
                    report.coverage.issue(issue);
                }
            }
        }
        report.coverage.complete_catalog = complete;
        let covered_ids: Vec<_> = records.iter().map(|r| r.session_id.clone()).collect();
        if cancelled(cancel) {
            report.status = ActivityStatus::Cancelled;
            if !report
                .coverage
                .issues
                .contains_key(&CoverageIssue::Cancelled)
            {
                report.coverage.issue(CoverageIssue::Cancelled);
            }
        } else if let Some(id) = selected_id {
            records.retain(|record| record.session_id.starts_with(id));
            report.status = if !complete {
                ActivityStatus::FullIdRequired
            } else {
                match records.len() {
                    0 => ActivityStatus::SessionNotFound,
                    1 => ActivityStatus::Ready,
                    _ => ActivityStatus::AmbiguousSession,
                }
            };
        } else if records.is_empty() {
            report.status = ActivityStatus::NoObservations;
        }
        records.sort_by(|a, b| {
            b.last_observed
                .cmp(&a.last_observed)
                .then_with(|| a.session_id.cmp(&b.session_id))
        });
        // Report construction is bounded by the aggregate read envelope, then
        // the serial output envelope; no scan result silently becomes current.
        report.sessions = records
            .into_iter()
            .map(|r| project(r, request.details))
            .collect();
        if complete {
            shorten_display_ids(&mut report.sessions, &covered_ids);
        }
        bound_output(&mut report, cancel);
        Ok(report)
    }
}

pub(super) fn report(
    manifest: &Path,
    cwd: &Path,
    request: ActivityRequest,
    cancel: &AtomicBool,
) -> Result<ActivityReport, ActivityError> {
    // Invalid requests are refused even when configuration is missing; no
    // filesystem operation is needed to discover an argument error.
    parse_selector(request.session.as_deref())?;
    if cancelled(cancel) {
        let mut coverage = Coverage::default();
        coverage.issue(CoverageIssue::Cancelled);
        return Ok(ActivityReport {
            schema: REPORT_SCHEMA,
            status: ActivityStatus::Cancelled,
            scope: None,
            sessions: Vec::new(),
            coverage,
        });
    }
    PreparedActivity::new(manifest, cwd)?.report_with_cancel(request, cancel)
}

fn valid_manifest(manifest: &Manifest) -> bool {
    if !manifest.root.to_str().is_some_and(absolute_path)
        || !manifest
            .device
            .as_deref()
            .is_none_or(|s| bounded_text(s, 128))
        || manifest.workspaces.is_empty()
        || manifest.workspaces.len() > 256
    {
        return false;
    }
    let mut paths = std::collections::BTreeSet::new();
    manifest.workspaces.iter().all(|binding| {
        binding.path.to_str().is_some_and(absolute_path)
            && binding.path.components().collect::<PathBuf>().as_os_str()
                == binding.path.as_os_str()
            && machine_code(&binding.source, 32)
            && paths.insert(&binding.path)
    })
}

fn parse_selector(selector: Option<&str>) -> Result<Option<(Option<&str>, &str)>, ActivityError> {
    let Some(selector) = selector else {
        return Ok(None);
    };
    if selector.len() > MAX_SESSION_SELECTOR_BYTES
        || !bounded_text(selector, MAX_SESSION_SELECTOR_BYTES)
    {
        return Err(ActivityError::invalid_selector());
    }
    if let Some((source, id)) = selector.split_once(':') {
        if !machine_code(source, 32) || !bounded_text(id, MAX_SESSION_ID_BYTES) {
            return Err(ActivityError::invalid_selector());
        }
        Ok(Some((Some(source), id)))
    } else if bounded_text(selector, MAX_SESSION_ID_BYTES) {
        Ok(Some((None, selector)))
    } else {
        Err(ActivityError::invalid_selector())
    }
}

fn read_record(
    root: &Directory,
    name: &str,
    source: &str,
    workspace: &str,
    expected_id: Option<&str>,
    coverage: &mut Coverage,
) -> Result<SessionRecord, CoverageIssue> {
    if coverage
        .read_budget_used
        .saturating_add(MAX_RECORD_BYTES + 1)
        > MAX_READ_BYTES
    {
        return Err(CoverageIssue::ReadLimit);
    }
    let before = coverage.read_bytes;
    let result = root.read(name, MAX_RECORD_BYTES, &mut coverage.read_bytes);
    coverage.read_budget_used += coverage.read_bytes - before;
    let bytes = result?;
    let record = decode_record(&bytes)?;
    if record_filename(&record.source, &record.session_id) != name
        || expected_id.is_some_and(|id| record.session_id != id)
    {
        return Err(CoverageIssue::IdentityMismatch);
    }
    if record.source != source || record.workspace != workspace {
        return Err(CoverageIssue::ScopeMismatch);
    }
    coverage.verified_records += 1;
    Ok(record)
}

fn shorten_display_ids(sessions: &mut [SessionActivity], ids: &[String]) {
    for session in sessions {
        let id = &session.session_id;
        let min = id.len().min(6);
        let length = (min..=id.len())
            .find(|&len| {
                id.is_char_boundary(len)
                    && !ids
                        .iter()
                        .any(|other| other != id && other.starts_with(&id[..len]))
            })
            .unwrap_or(id.len());
        session.display_id = id[..length].to_owned();
    }
}

struct Candidate {
    name: String,
    modified: Option<std::time::SystemTime>,
}
struct Catalog {
    candidates: Vec<Candidate>,
    inspected: usize,
    issues: Vec<CoverageIssue>,
    complete: bool,
}

fn read_bounded(
    mut file: File,
    max: usize,
    read_bytes: &mut usize,
) -> Result<Vec<u8>, CoverageIssue> {
    let metadata = file.metadata().map_err(|_| CoverageIssue::Unreadable)?;
    if !metadata.is_file() {
        return Err(CoverageIssue::UnsafePath);
    }
    if metadata.len() > max as u64 {
        return Err(CoverageIssue::Oversize);
    }
    let mut bytes = Vec::with_capacity(metadata.len() as usize);
    let result = file.by_ref().take(max as u64 + 1).read_to_end(&mut bytes);
    // A short failed read and a file that grew after metadata still consume the
    // aggregate allowance. Charge before propagating either failure.
    *read_bytes += bytes.len();
    result.map_err(|_| CoverageIssue::Unreadable)?;
    if bytes.len() > max {
        return Err(CoverageIssue::Oversize);
    }
    Ok(bytes)
}

#[cfg(unix)]
use unix::{Directory, read_absolute};
#[cfg(not(unix))]
use unsupported::{Directory, read_absolute};

#[cfg(unix)]
mod unix {
    use super::*;
    use std::ffi::{CStr, CString, OsStr};
    use std::os::fd::{AsRawFd, FromRawFd, IntoRawFd};
    use std::os::unix::ffi::OsStrExt;
    use std::path::Component;

    pub(super) struct Directory {
        file: File,
    }

    fn classify(error: io::Error) -> CoverageIssue {
        match error.raw_os_error() {
            Some(libc::ELOOP | libc::ENOTDIR) => CoverageIssue::UnsafePath,
            _ if error.kind() == io::ErrorKind::NotFound => CoverageIssue::Missing,
            _ => CoverageIssue::Unreadable,
        }
    }

    fn open_at(parent: &File, name: &OsStr, directory: bool) -> Result<File, CoverageIssue> {
        let name = CString::new(name.as_bytes()).map_err(|_| CoverageIssue::UnsafePath)?;
        let mut flags = libc::O_RDONLY | libc::O_CLOEXEC | libc::O_NOFOLLOW | libc::O_NONBLOCK;
        if directory {
            flags |= libc::O_DIRECTORY;
        }
        // SAFETY: parent is a live descriptor and name is NUL terminated. openat
        // borrows both; on success the new descriptor is uniquely owned below.
        let fd = unsafe { libc::openat(parent.as_raw_fd(), name.as_ptr(), flags) };
        if fd < 0 {
            return Err(classify(io::Error::last_os_error()));
        }
        // SAFETY: successful openat returned a new owned descriptor.
        let file = unsafe { File::from_raw_fd(fd) };
        let metadata = file.metadata().map_err(|_| CoverageIssue::Unreadable)?;
        if (directory && !metadata.is_dir()) || (!directory && !metadata.is_file()) {
            return Err(CoverageIssue::UnsafePath);
        }
        Ok(file)
    }

    fn directory_path(path: &Path) -> Result<File, CoverageIssue> {
        if !path.is_absolute() {
            return Err(CoverageIssue::UnsafePath);
        }
        let root = CString::new("/").expect("literal");
        // SAFETY: the literal is a valid C path. Ownership follows successful open.
        let fd = unsafe {
            libc::open(
                root.as_ptr(),
                libc::O_RDONLY | libc::O_CLOEXEC | libc::O_DIRECTORY | libc::O_NOFOLLOW,
            )
        };
        if fd < 0 {
            return Err(classify(io::Error::last_os_error()));
        }
        // SAFETY: successful open returned a new owned descriptor.
        let mut current = unsafe { File::from_raw_fd(fd) };
        for part in path.components() {
            match part {
                Component::RootDir | Component::CurDir => {}
                Component::Normal(name) => current = open_at(&current, name, true)?,
                Component::ParentDir | Component::Prefix(_) => {
                    return Err(CoverageIssue::UnsafePath);
                }
            }
        }
        Ok(current)
    }

    pub(super) fn read_absolute(path: &Path, max: usize) -> Result<Vec<u8>, CoverageIssue> {
        let parent = path.parent().ok_or(CoverageIssue::UnsafePath)?;
        let name = path.file_name().ok_or(CoverageIssue::UnsafePath)?;
        let directory = directory_path(parent)?;
        read_bounded(open_at(&directory, name, false)?, max, &mut 0)
    }

    impl Directory {
        pub(super) fn open(path: &Path) -> Result<Self, CoverageIssue> {
            directory_path(path).map(|file| Self { file })
        }
        pub(super) fn read(
            &self,
            name: &str,
            max: usize,
            read_bytes: &mut usize,
        ) -> Result<Vec<u8>, CoverageIssue> {
            if name.contains('/') || name == "." || name == ".." {
                return Err(CoverageIssue::UnsafePath);
            }
            read_bounded(
                open_at(&self.file, OsStr::new(name), false)?,
                max,
                read_bytes,
            )
        }
        pub(super) fn catalog(&self, cancel: &AtomicBool) -> Catalog {
            let mut result = Catalog {
                candidates: Vec::new(),
                inspected: 0,
                issues: Vec::new(),
                complete: false,
            };
            let file = match open_at(&self.file, OsStr::new("."), true) {
                Ok(file) => file,
                Err(issue) => {
                    result.issues.push(issue);
                    return result;
                }
            };
            let fd = file.into_raw_fd();
            // SAFETY: fd is an owned directory descriptor. fdopendir takes its
            // ownership on success; on failure the descriptor remains ours.
            let stream = unsafe { libc::fdopendir(fd) };
            if stream.is_null() {
                // SAFETY: fdopendir failed, retaining the owned descriptor.
                drop(unsafe { File::from_raw_fd(fd) });
                result.issues.push(CoverageIssue::Unreadable);
                return result;
            }
            struct Stream(
                // Safety invariant: a non-null live fdopendir result uniquely
                // owned here; only this loop accesses it, and Drop closes it.
                *mut libc::DIR,
            );
            impl Drop for Stream {
                fn drop(&mut self) {
                    // SAFETY: Stream owns this successful fdopendir result and
                    // closes it exactly once, after all readdir borrows end.
                    unsafe {
                        libc::closedir(self.0);
                    }
                }
            }
            let stream = Stream(stream);
            loop {
                if cancelled(cancel) {
                    result.issues.push(CoverageIssue::Cancelled);
                    break;
                }
                clear_errno();
                // SAFETY: stream is live and solely used by this loop; the
                // returned dirent is consumed before the next readdir call.
                let entry = unsafe { libc::readdir(stream.0) };
                if entry.is_null() {
                    if io::Error::last_os_error().raw_os_error() == Some(0) {
                        result.complete = true;
                    } else {
                        result.issues.push(CoverageIssue::Unreadable);
                    }
                    break;
                }
                // SAFETY: readdir supplied a non-null live dirent. Project only
                // the field address, without borrowing the platform's declared
                // array length (the actual dirent allocation may be shorter).
                let name_ptr =
                    unsafe { std::ptr::addr_of!((*entry).d_name) }.cast::<libc::c_char>();
                // SAFETY: readdir's d_name is initialized through its NUL in one
                // live directory buffer; its OS-bounded name fits isize::MAX.
                // CStr's borrow ends before the next readdir/closedir on this
                // exclusively owned stream, the only operations invalidating it.
                let name = unsafe { CStr::from_ptr(name_ptr) }.to_bytes();
                if name == b"." || name == b".." {
                    continue;
                }
                if result.inspected == MAX_DIRECTORY_ENTRIES {
                    result.issues.push(CoverageIssue::DirectoryLimit);
                    break;
                }
                result.inspected += 1;
                if matches!(
                    name,
                    b".activity.lock" | b".activity-quota.json" | b".activity-stage"
                ) {
                    continue;
                }
                let Ok(name) = std::str::from_utf8(name) else {
                    result.issues.push(CoverageIssue::UnknownEntry);
                    continue;
                };
                let valid_name = name
                    .strip_suffix(".json")
                    .and_then(|s| s.rsplit_once('-'))
                    .is_some_and(|(source, digest)| {
                        machine_code(source, 32)
                            && digest.len() == 64
                            && digest
                                .bytes()
                                .all(|c| c.is_ascii_digit() || (b'a'..=b'f').contains(&c))
                    });
                if !valid_name {
                    result.issues.push(CoverageIssue::UnknownEntry);
                    continue;
                }
                // Open for metadata with the same no-follow regular-file check
                // used for decoding. No bytes are read during candidate ordering.
                match open_at(&self.file, OsStr::new(name), false) {
                    Ok(file) => result.candidates.push(Candidate {
                        name: name.to_owned(),
                        modified: file.metadata().ok().and_then(|m| m.modified().ok()),
                    }),
                    Err(issue) => result.issues.push(issue),
                }
            }
            // A rejected candidate may have held a matching ID; don't bless a
            // prefix as unique merely because that candidate was unreadable.
            if result
                .issues
                .iter()
                .any(|issue| *issue != CoverageIssue::UnknownEntry)
            {
                result.complete = false;
            }
            result
        }
    }

    fn clear_errno() {
        // SAFETY: these platform functions return the current thread's errno
        // location, which is writable by this thread until the next syscall.
        #[cfg(any(target_os = "linux", target_os = "android"))]
        unsafe {
            *libc::__errno_location() = 0;
        }
        #[cfg(any(
            target_os = "macos",
            target_os = "ios",
            target_os = "freebsd",
            target_os = "dragonfly"
        ))]
        unsafe {
            *libc::__error() = 0;
        }
        #[cfg(any(target_os = "netbsd", target_os = "openbsd"))]
        unsafe {
            *libc::__errno() = 0;
        }
        #[cfg(any(target_os = "solaris", target_os = "illumos"))]
        unsafe {
            *libc::___errno() = 0;
        }
    }
}

#[cfg(not(unix))]
mod unsupported {
    use super::*;
    pub(super) struct Directory;
    pub(super) fn read_absolute(_: &Path, _: usize) -> Result<Vec<u8>, CoverageIssue> {
        Err(CoverageIssue::PlatformUnsupported)
    }
    impl Directory {
        pub(super) fn open(_: &Path) -> Result<Self, CoverageIssue> {
            Err(CoverageIssue::PlatformUnsupported)
        }
        pub(super) fn read(
            &self,
            _: &str,
            _: usize,
            _: &mut usize,
        ) -> Result<Vec<u8>, CoverageIssue> {
            Err(CoverageIssue::PlatformUnsupported)
        }
        pub(super) fn catalog(&self, _: &AtomicBool) -> Catalog {
            Catalog {
                candidates: Vec::new(),
                inspected: 0,
                issues: vec![CoverageIssue::PlatformUnsupported],
                complete: false,
            }
        }
    }
}
