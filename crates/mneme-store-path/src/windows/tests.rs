use super::*;
use std::io::{Read, Write};

fn root() -> Directory {
    Directory::create(&std::env::temp_dir().join(format!("mneme-windows-{}", ulid::Ulid::new())))
        .unwrap()
}

#[test]
fn leaf_and_literal_path_validation_refuses_windows_aliases() {
    for name in [
        "",
        ".",
        "..",
        "a/b",
        "a\\b",
        "a:b",
        "a.",
        "a ",
        "NUL",
        "nul.txt",
        "COM1",
        "lpt9.log",
        "COM¹.txt",
        "LPT²",
        "AUX",
        "a?",
        "a*",
    ] {
        assert!(validate_leaf(OsStr::new(name)).is_err(), "{name:?}");
    }
    for path in [
        "C:relative",
        "\\rooted",
        "\\\\server\\share\\file",
        "\\\\.\\C:\\file",
        "C:\\a\\..\\b",
        "C:\\a\\.\\b",
        "C:\\a\\\\b",
        "C:\\a\\b\\",
        "C:\\a:b",
        "C:\\NUL.txt",
    ] {
        assert!(validate_absolute_path(Path::new(path)).is_err(), "{path:?}");
    }
    for path in [
        "C:\\",
        "C:\\ordinary\\memory.db",
        "C:/ordinary/memory.db",
        "\\\\?\\C:\\ordinary\\memory.db",
    ] {
        validate_absolute_path(Path::new(path)).unwrap();
    }
    validate_leaf(OsStr::new("雪-memory.db")).unwrap();
    for path in ["part/../file", "part/./file", "./part", "part//file"] {
        assert!(
            absolute_disk_path(Path::new(path)).is_err(),
            "relative {path:?}"
        );
    }
}

#[test]
fn publication_is_exact_and_no_clobber() {
    let root = root();
    let mut source = root.create_file(OsStr::new("stage")).unwrap();
    source.write_all(b"retained bytes").unwrap();
    source.sync_all().unwrap();
    let id = file_identity(&source).unwrap();
    let mut competitor = root.create_file(OsStr::new("occupied")).unwrap();
    competitor.write_all(b"unrelated bytes").unwrap();
    assert!(rename_no_replace(&source, &root, OsStr::new("occupied")).is_err());
    assert_eq!(
        file_identity(&root.open_file(OsStr::new("stage"), false).unwrap()).unwrap(),
        id
    );
    rename_no_replace(&source, &root, OsStr::new("published")).unwrap();
    source.sync_all().unwrap();
    root.sync_all().unwrap();
    let mut published = root.open_file(OsStr::new("published"), false).unwrap();
    assert_eq!(file_identity(&published).unwrap(), id);
    let mut contents = String::new();
    published.read_to_string(&mut contents).unwrap();
    assert_eq!(contents, "retained bytes");
    drop((source, competitor, published));
    std::fs::remove_dir_all(root.path()).unwrap();
}

#[test]
fn directory_publication_is_no_clobber_and_preserves_full_identity() {
    let root = root();
    let stage = root.create_directory(OsStr::new("stage")).unwrap();
    let file = stage.create_file(OsStr::new("member")).unwrap();
    file.sync_all().unwrap();
    stage.sync_all().unwrap();
    root.create_directory(OsStr::new("occupied")).unwrap();
    assert!(rename_directory_no_replace(&stage, &root, OsStr::new("occupied")).is_err());
    stage.require_named().unwrap();
    rename_directory_no_replace(&stage, &root, OsStr::new("published")).unwrap();
    stage.sync_all().unwrap();
    root.sync_all().unwrap();
    assert!(stage.require_named().is_err());
    let published = root.open_directory(OsStr::new("published")).unwrap();
    assert!(published.identity().same_object(stage.identity()));
    assert!(published.open_file(OsStr::new("member"), false).is_ok());
    drop((published, stage, file));
    std::fs::remove_dir_all(root.path()).unwrap();
}

#[test]
fn removal_refuses_replacement_and_retains_unrelated_data() {
    let root = root();
    let first = root.create_file(OsStr::new("name")).unwrap();
    let first_id = file_identity(&first).unwrap();
    rename_no_replace(&first, &root, OsStr::new("moved")).unwrap();
    let mut second = root.create_file(OsStr::new("name")).unwrap();
    second.write_all(b"unrelated").unwrap();
    assert!(root.remove_file(OsStr::new("name"), first_id).is_err());
    root.remove_file(OsStr::new("name"), file_identity(&second).unwrap())
        .unwrap();
    assert_eq!(
        root.open_file(OsStr::new("name"), false)
            .unwrap_err()
            .kind(),
        io::ErrorKind::NotFound
    );
    // The original name's retained handle cannot make deletion hit a replacement.
    assert!(root.open_file(OsStr::new("moved"), false).is_ok());
    drop((first, second));
    std::fs::remove_dir_all(root.path()).unwrap();
}

#[test]
fn hardlinked_regular_files_are_refused() {
    let root = root();
    let file = root.create_file(OsStr::new("one")).unwrap();
    std::fs::hard_link(root.path().join("one"), root.path().join("two")).unwrap();
    assert!(file_identity(&file).is_err());
    assert!(root.open_file(OsStr::new("one"), false).is_err());
    assert!(crate::StoreLease::acquire(&root.path().join("one")).is_err());
    drop(file);
    std::fs::remove_dir_all(root.path()).unwrap();
}

#[test]
fn lease_is_exclusive_released_and_pinned_to_original_lock() {
    let root = root();
    let target = root.path().join("memory.db");
    let first = crate::StoreLease::acquire(&target).unwrap();
    assert_eq!(
        crate::StoreLease::acquire(&target).unwrap_err().kind(),
        io::ErrorKind::WouldBlock
    );
    first.require_guards(&target).unwrap();
    let lock_name = first.lock_path.file_name().unwrap();
    rename_no_replace(&first.file, &root, OsStr::new("old-lock")).unwrap();
    let _replacement = root.create_file(lock_name).unwrap();
    assert!(first.require_guards(&target).is_err());
    drop(first);
    let next = crate::StoreLease::acquire(&target).unwrap();
    next.require_guards(&target).unwrap();
    drop((next, _replacement));
    std::fs::remove_dir_all(root.path()).unwrap();
}

#[test]
fn case_aliases_and_parent_rename_preserve_lease_exclusion() {
    let root = root();
    let parent = root.create_directory(OsStr::new("parent")).unwrap();
    let target = parent.path().join("memory.db");
    let first = crate::StoreLease::acquire(&target).unwrap();
    assert_eq!(
        crate::StoreLease::acquire(&parent.path().join("MEMORY.DB"))
            .unwrap_err()
            .kind(),
        io::ErrorKind::WouldBlock
    );
    let renamed = root.path().join("renamed");
    std::fs::rename(parent.path(), &renamed).unwrap();
    assert!(first.require_guards(&target).is_err());
    assert_eq!(
        crate::StoreLease::acquire(&renamed.join("memory.db"))
            .unwrap_err()
            .kind(),
        io::ErrorKind::WouldBlock
    );
    drop(first);
    let next = crate::StoreLease::acquire(&renamed.join("memory.db")).unwrap();
    next.require_guards(&renamed.join("memory.db")).unwrap();
    drop((next, parent));
    std::fs::remove_dir_all(root.path()).unwrap();
}

#[test]
fn lease_child_process() {
    let Some(target) = std::env::var_os("MNEME_TEST_WINDOWS_LEASE_TARGET") else {
        return;
    };
    let ready = std::env::var_os("MNEME_TEST_WINDOWS_LEASE_READY").unwrap();
    let _lease = crate::StoreLease::acquire(Path::new(&target)).unwrap();
    std::fs::write(ready, b"lease-held").unwrap();
    // Parent deliberately kills this process, testing OS release rather than
    // cooperative Drop. Bound a failed harness so no orphan runs forever.
    std::thread::sleep(std::time::Duration::from_secs(120));
    panic!("parent failed to finish lease crash test");
}

#[test]
fn lease_excludes_other_processes_and_releases_after_process_death() {
    struct Child(std::process::Child);
    impl Drop for Child {
        fn drop(&mut self) {
            let _ = self.0.kill();
            let _ = self.0.wait();
        }
    }
    let root = root();
    let target = root.path().join("memory.db");
    let ready = root.path().join("ready");
    let mut child = Child(
        std::process::Command::new(std::env::current_exe().unwrap())
            .args([
                "--exact",
                "windows::tests::lease_child_process",
                "--nocapture",
            ])
            .env("MNEME_TEST_WINDOWS_LEASE_TARGET", &target)
            .env("MNEME_TEST_WINDOWS_LEASE_READY", &ready)
            .spawn()
            .unwrap(),
    );
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(20);
    while !ready.exists() {
        assert!(
            child.0.try_wait().unwrap().is_none(),
            "lease child exited before ready"
        );
        assert!(
            std::time::Instant::now() < deadline,
            "lease child did not become ready"
        );
        std::thread::sleep(std::time::Duration::from_millis(10));
    }
    assert_eq!(
        crate::StoreLease::acquire(&target).unwrap_err().kind(),
        io::ErrorKind::WouldBlock
    );
    child.0.kill().unwrap();
    child.0.wait().unwrap();
    let lease = crate::StoreLease::acquire(&target).unwrap();
    lease.require_guards(&target).unwrap();
    drop(lease);
    std::fs::remove_dir_all(root.path()).unwrap();
}

#[test]
fn directory_namespace_swap_invalidates_guard_without_redirecting_child_open() {
    let root = root();
    let stage = root.create_directory(OsStr::new("stage")).unwrap();
    stage.create_file(OsStr::new("original")).unwrap();
    // The test's path-based rename simulates another same-user actor. Real
    // mutation helpers never resolve their child through this stale pathname.
    std::fs::rename(stage.path(), root.path().join("moved")).unwrap();
    root.create_directory(OsStr::new("stage")).unwrap();
    assert!(stage.require_named().is_err());
    assert!(stage.open_file(OsStr::new("original"), false).is_ok());
    assert!(stage.create_file(OsStr::new("must-not-create")).is_err());
    drop(stage);
    std::fs::remove_dir_all(root.path()).unwrap();
}

#[test]
fn no_reparse_point_is_followed_even_in_an_intermediate_component() {
    let root = root();
    let target = root.create_directory(OsStr::new("target")).unwrap();
    // Creating symlinks requires Developer Mode or SeCreateSymbolicLinkPrivilege.
    // A nonprivileged machine cannot run this fixture; all other tests remain
    // mandatory. CI should enable Developer Mode for this native boundary.
    let link = root.path().join("link");
    if let Err(error) = std::os::windows::fs::symlink_dir(target.path(), &link) {
        if error.raw_os_error() == Some(1314) {
            eprintln!("SKIPPED symlink fixture: Developer Mode or symlink privilege unavailable");
            return;
        }
        panic!("cannot construct symlink fixture: {error}");
    }
    assert!(Directory::open(&link).is_err());
    target.create_directory(OsStr::new("nested")).unwrap();
    assert!(Directory::open(&link.join("nested")).is_err());
    drop(target);
    std::fs::remove_dir(&link).unwrap();
    std::fs::remove_dir_all(root.path()).unwrap();
}

#[test]
fn short_path_aliases_share_the_same_adjacent_lease() {
    use std::os::windows::ffi::OsStringExt;
    use windows_sys::Win32::Storage::FileSystem::GetShortPathNameW;
    let root = root();
    let target = root.path().join("a-long-memory-database-name.db");
    drop(
        root.create_database_file(OsStr::new("a-long-memory-database-name.db"))
            .unwrap(),
    );
    let lease = crate::StoreLease::acquire(&target).unwrap();
    let source: Vec<u16> = target.as_os_str().encode_wide().chain(Some(0)).collect();
    let mut short = vec![0u16; 32768];
    // SAFETY: source is a live NUL-terminated UTF-16 pathname without internal
    // NUL. short is an exclusive initialized writable 32768-unit buffer, and its
    // exact capacity is passed. GetShortPathNameW is synchronous and retains no
    // pointers. The returned length is checked before slicing.
    let count =
        unsafe { GetShortPathNameW(source.as_ptr(), short.as_mut_ptr(), short.len() as u32) };
    assert!(
        count != 0,
        "GetShortPathNameW: {}",
        io::Error::last_os_error()
    );
    assert!((count as usize) < short.len());
    let alias = PathBuf::from(OsString::from_wide(&short[..count as usize]));
    if alias == target {
        eprintln!("8.3 name generation is disabled on this volume; checking returned alias only");
    }
    assert_eq!(
        crate::StoreLease::acquire(&alias).unwrap_err().kind(),
        io::ErrorKind::WouldBlock
    );
    drop(lease);
    let reacquired = crate::StoreLease::acquire(&alias).unwrap();
    reacquired.require_guards(&alias).unwrap();
    drop(reacquired);
    std::fs::remove_dir_all(root.path()).unwrap();
}

#[test]
fn file_revision_tracks_full_identity_but_read_access_does_not_invalidate_it() {
    let root = root();
    let mut writer = root.create_file(OsStr::new("revision")).unwrap();
    writer.write_all(b"first").unwrap();
    writer.sync_all().unwrap();
    drop(writer);
    let mut reader = root.open_file(OsStr::new("revision"), false).unwrap();
    let before = file_revision(&reader).unwrap();
    let mut bytes = Vec::new();
    reader.read_to_end(&mut bytes).unwrap();
    assert_eq!(bytes, b"first");
    assert_eq!(file_revision(&reader).unwrap(), before);
    let mut writer = root.open_file(OsStr::new("revision"), true).unwrap();
    writer.write_all(b"different length").unwrap();
    writer.sync_all().unwrap();
    drop(writer);
    let after = file_revision(&reader).unwrap();
    assert!(after.identity.same_file(before.identity));
    assert_ne!(after, before);
    drop(reader);
    std::fs::remove_dir_all(root.path()).unwrap();
}
