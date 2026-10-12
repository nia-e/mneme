use super::*;
use crate::FsStore;
use mneme_core::ports::BodyStore;
use std::path::PathBuf;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) enum PublicationCut {
    BeforeWrite,
    AfterWrite,
    AfterFileSync,
    BeforeRename,
    AfterRename,
    AfterDirectorySync,
    AfterReadback,
}

thread_local! {
    static PUBLICATION_CUT: std::cell::Cell<Option<PublicationCut>> = const { std::cell::Cell::new(None) };
}

pub(super) fn publication_cut(cut: PublicationCut) -> io::Result<()> {
    if PUBLICATION_CUT.get() == Some(cut) {
        Err(io::Error::other(format!(
            "injected body publication failure: {cut:?}"
        )))
    } else {
        Ok(())
    }
}

struct Scratch(PathBuf);

impl Scratch {
    fn new() -> Self {
        let path = std::env::temp_dir().join(format!("mneme-body-windows-{}", Ulid::new()));
        Directory::create(&path).unwrap();
        Self(path)
    }
}

impl Drop for Scratch {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

fn entries(path: &Path) -> Vec<OsString> {
    let mut names = std::fs::read_dir(path)
        .unwrap()
        .map(|entry| entry.unwrap().file_name())
        .collect::<Vec<_>>();
    names.sort();
    names
}

#[test]
fn absent_existing_root_does_not_create_or_change_parent() {
    let scratch = Scratch::new();
    let parent = Directory::open(&scratch.0).unwrap();
    let before = parent.identity();
    let missing = scratch.0.join("absent").join("bodies");
    let before_entries = entries(&scratch.0);

    assert!(FsStore::open_existing(&missing).is_err());

    assert!(!missing.exists());
    assert!(!scratch.0.join("absent").exists());
    assert_eq!(entries(&scratch.0), before_entries);
    assert_eq!(Directory::open(&scratch.0).unwrap().identity(), before);
}

#[test]
fn creating_root_rejects_drive_relative_and_empty_spellings() {
    assert!(FsStore::new(Path::new("")).is_err());
    let name = format!("mneme-ambiguous-body-root-{}", Ulid::new());
    assert!(FsStore::new(Path::new(&format!("C:{name}"))).is_err());
    assert!(FsStore::new(Path::new(&format!(r"\{name}"))).is_err());
    let scratch = Scratch::new();
    for path in [
        scratch.0.join("missing").join("..").join("bodies"),
        scratch.0.join(".").join("bodies"),
    ] {
        assert!(FsStore::new(&path).is_err());
        assert!(FsStore::open_existing(&path).is_err());
        assert!(entries(&scratch.0).is_empty());
    }
}

#[tokio::test]
async fn existing_root_reopens_and_reads_body_without_namespace_changes() {
    let scratch = Scratch::new();
    let path = scratch.0.join("nested").join("bodies");
    let store = FsStore::new(&path).unwrap();
    let body = store.put(b"whole body before publication").await.unwrap();
    let before_entries = entries(&path);
    let before = Directory::open(&path).unwrap().identity();
    drop(store);

    let reopened = FsStore::open_existing(&path).unwrap();
    assert_eq!(
        reopened.get(&body).await.unwrap(),
        b"whole body before publication"
    );
    assert_eq!(
        reopened.get_range(&body, 6, 4).await.unwrap().bytes,
        b"body"
    );
    assert_eq!(entries(&path), before_entries);
    assert_eq!(Directory::open(&path).unwrap().identity(), before);
}

#[tokio::test]
async fn legacy_absolute_immediate_child_uses_the_admitted_root() {
    let scratch = Scratch::new();
    let store = FsStore::new(scratch.0.join("bodies")).unwrap();
    let body = store
        .root
        .put_named("ordinary-name.txt", b"legacy")
        .unwrap();
    let absolute = store.root.directory.path().join("ordinary-name.txt");
    let legacy = BodyRef::new(format!("fs://{}", absolute.display())).unwrap();

    assert_eq!(store.get(&legacy).await.unwrap(), b"legacy");
    let case_alias = store
        .root
        .directory
        .path()
        .with_file_name("BODIES")
        .join("ordinary-name.txt");
    let case_alias = BodyRef::new(format!("fs://{}", case_alias.display())).unwrap();
    assert_eq!(store.get(&case_alias).await.unwrap(), b"legacy");
    store.delete(&legacy).await.unwrap();
    assert!(store.get(&body).await.is_err());
    store.delete(&legacy).await.unwrap();
}

#[tokio::test]
async fn unsafe_and_ambiguous_ref_spellings_never_reach_another_file() {
    let scratch = Scratch::new();
    let store = FsStore::new(scratch.0.join("bodies")).unwrap();
    let body = store.root.put_named("body", b"unchanged").unwrap();
    let before_entries = entries(store.root.directory.path());
    for raw in [
        r"C:body",
        r"\body",
        "/body",
        r".\body",
        r"body\.",
        "body/",
        "body:stream",
        "body::$DATA",
        "body.",
        "body ",
        "NUL.txt",
        "COM¹",
        r"..\body",
        r"\\server\share\body",
        r"\\.\NUL",
    ] {
        let invalid = BodyRef::new(format!("fs://{raw}")).unwrap();
        assert!(store.get(&invalid).await.is_err(), "get accepted {raw:?}");
        assert!(
            store.get_range(&invalid, 0, 1).await.is_err(),
            "range accepted {raw:?}"
        );
        assert!(
            store.delete(&invalid).await.is_err(),
            "delete accepted {raw:?}"
        );
    }
    assert_eq!(entries(store.root.directory.path()), before_entries);
    assert_eq!(store.get(&body).await.unwrap(), b"unchanged");

    // Percent escapes are ordinary filename text, not URL path decoding.
    let literal = store.root.put_named("%2e%2e", b"literal").unwrap();
    assert_eq!(store.get(&literal).await.unwrap(), b"literal");
    let unicode = store.root.put_named("記憶 & %.txt", b"unicode").unwrap();
    assert_eq!(store.get(&unicode).await.unwrap(), b"unicode");
}

#[tokio::test]
async fn nested_and_outside_absolute_refs_are_rejected_without_deletion() {
    let scratch = Scratch::new();
    let store = FsStore::new(scratch.0.join("bodies")).unwrap();
    let nested = store
        .root
        .directory
        .create_directory(OsStr::new("nested"))
        .unwrap();
    let mut nested_file = nested.create_file(OsStr::new("body")).unwrap();
    nested_file.write_all(b"nested").unwrap();
    nested_file.sync_all().unwrap();
    let outside = Root::create_or_open(&scratch.0.join("outside")).unwrap();
    outside.put_named("body", b"outside").unwrap();

    for path in [
        nested.path().join("body"),
        outside.directory.path().join("body"),
    ] {
        let body = BodyRef::new(format!("fs://{}", path.display())).unwrap();
        let before = std::fs::read(&path).unwrap();
        assert!(store.get(&body).await.is_err());
        assert!(store.get_range(&body, 0, 1).await.is_err());
        assert!(store.delete(&body).await.is_err());
        assert_eq!(std::fs::read(&path).unwrap(), before);
    }
}

#[tokio::test]
async fn multiply_linked_bodies_are_not_read_or_deleted() {
    let scratch = Scratch::new();
    let store = FsStore::new(scratch.0.join("bodies")).unwrap();
    let body = store.root.put_named("body", b"shared inode").unwrap();
    let original = store.root.directory.path().join("body");
    let alias = scratch.0.join("alias");
    std::fs::hard_link(&original, &alias).unwrap();

    assert!(store.get(&body).await.is_err());
    assert!(store.get_range(&body, 0, 1).await.is_err());
    assert!(store.delete(&body).await.is_err());
    assert_eq!(std::fs::read(&original).unwrap(), b"shared inode");
    assert_eq!(std::fs::read(&alias).unwrap(), b"shared inode");
}

#[test]
fn body_publication_is_no_clobber_and_cleans_only_its_temporary() {
    let scratch = Scratch::new();
    let root = Root::create_or_open(&scratch.0.join("bodies")).unwrap();
    root.put_named("body", b"first").unwrap();
    let before_entries = entries(root.directory.path());

    assert!(root.put_named("body", b"second").is_err());

    assert_eq!(
        std::fs::read(root.directory.path().join("body")).unwrap(),
        b"first"
    );
    assert_eq!(entries(root.directory.path()), before_entries);
}

#[test]
fn replaced_body_root_is_refused_without_writing_to_replacement() {
    let scratch = Scratch::new();
    let path = scratch.0.join("bodies");
    let old_path = scratch.0.join("old-bodies");
    let root = Root::create_or_open(&path).unwrap();
    root.put_named("body", b"old object").unwrap();
    std::fs::rename(&path, &old_path).unwrap();
    Directory::create(&path).unwrap();

    assert!(root.put(b"must not escape").is_err());
    assert!(root.open_file(&BodyRef::new("fs://body").unwrap()).is_err());
    assert!(root.delete(&BodyRef::new("fs://body").unwrap()).is_err());
    assert!(entries(&path).is_empty());
    assert_eq!(std::fs::read(old_path.join("body")).unwrap(), b"old object");
}

#[test]
fn publication_failures_preserve_crossed_final_names_and_cleanup_only_staging() {
    for (cut, crossed) in [
        (PublicationCut::BeforeWrite, false),
        (PublicationCut::AfterWrite, false),
        (PublicationCut::AfterFileSync, false),
        (PublicationCut::BeforeRename, false),
        (PublicationCut::AfterRename, true),
        (PublicationCut::AfterDirectorySync, true),
        (PublicationCut::AfterReadback, true),
    ] {
        let scratch = Scratch::new();
        let root = Root::create_or_open(&scratch.0.join("bodies")).unwrap();
        root.put_named("unrelated", b"preserve me").unwrap();
        PUBLICATION_CUT.set(Some(cut));
        let result = root.put_named("body", b"complete content");
        PUBLICATION_CUT.set(None);

        assert!(result.is_err(), "failure did not fire: {cut:?}");
        assert_eq!(
            std::fs::read(root.directory.path().join("unrelated")).unwrap(),
            b"preserve me"
        );
        let final_path = root.directory.path().join("body");
        if crossed {
            assert_eq!(std::fs::read(&final_path).unwrap(), b"complete content");
            assert_eq!(
                entries(root.directory.path()),
                [OsString::from("body"), OsString::from("unrelated")]
            );
        } else {
            assert!(!final_path.exists());
            assert_eq!(
                entries(root.directory.path()),
                [OsString::from("unrelated")]
            );
        }
    }
}

fn grant_everyone(path: &Path, rights: &str) {
    let output = std::process::Command::new("icacls.exe")
        .arg(path)
        .arg("/grant")
        .arg(format!("*S-1-1-0:{rights}"))
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "icacls fixture: {}",
        String::from_utf8_lossy(&output.stderr)
    );
}

#[test]
fn existing_root_with_permissive_acl_is_never_repaired() {
    let scratch = Scratch::new();
    let path = scratch.0.join("bodies");
    let root = Root::create_or_open(&path).unwrap();
    root.put_named("sentinel", b"preserve body and permissions")
        .unwrap();
    grant_everyone(&path, "(OI)(CI)F");
    let inspect_acl = || {
        std::process::Command::new("icacls.exe")
            .arg(&path)
            .output()
            .unwrap()
    };
    let before = inspect_acl();
    assert!(before.status.success());

    assert!(FsStore::open_existing(&path).is_err());
    assert!(FsStore::new(&path).is_err());

    let after = inspect_acl();
    assert!(after.status.success());
    assert_eq!(after.stdout, before.stdout);
    assert_eq!(entries(&path), [OsString::from("sentinel")]);
    assert_eq!(
        std::fs::read(path.join("sentinel")).unwrap(),
        b"preserve body and permissions"
    );
}

#[tokio::test]
async fn permissive_body_acl_is_not_read_or_deleted() {
    let scratch = Scratch::new();
    let store = FsStore::new(scratch.0.join("bodies")).unwrap();
    let body = store.root.put_named("body", b"private bytes").unwrap();
    let path = store.root.directory.path().join("body");
    grant_everyone(&path, "R");

    assert!(store.get(&body).await.is_err());
    assert!(store.get_range(&body, 0, 1).await.is_err());
    assert!(store.delete(&body).await.is_err());
    assert_eq!(std::fs::read(path).unwrap(), b"private bytes");
}

#[test]
fn existing_root_rejects_junctions_without_following_them() {
    let scratch = Scratch::new();
    let target = scratch.0.join("target");
    let alias = scratch.0.join("alias");
    Directory::create(&target).unwrap();
    let result = std::process::Command::new("cmd.exe")
        .args(["/D", "/C", "mklink", "/J"])
        .arg(&alias)
        .arg(&target)
        .output()
        .unwrap();
    assert!(
        result.status.success(),
        "mklink fixture: {}",
        String::from_utf8_lossy(&result.stderr)
    );

    assert!(FsStore::open_existing(&alias).is_err());
    assert!(FsStore::new(&alias).is_err());
    assert!(entries(&target).is_empty());
    // Remove the junction itself, not its target directory.
    std::fs::remove_dir(&alias).unwrap();
    assert!(target.is_dir());
}
