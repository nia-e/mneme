//! Native-Windows tests: these exercise NTFS handles/ACLs, not Unix emulation.

use super::*;
use crate::windows::{Directory, file_identity, require_private_file};
use std::fs::{self, OpenOptions};
use std::io::Write;
use std::os::windows::ffi::{OsStrExt, OsStringExt};

struct Scratch(PathBuf);

impl Scratch {
    fn new() -> Self {
        let path = std::env::temp_dir().join(format!("mneme-fresh-windows-{}", Ulid::new()));
        drop(Directory::create(&path).unwrap());
        Self(fs::canonicalize(path).unwrap())
    }

    fn target(&self) -> PathBuf {
        self.0.join("memory.db")
    }
}

impl Drop for Scratch {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

fn spec() -> FreshStoreStageSpecV1 {
    FreshStoreStageSpecV1 {
        operation_id: Ulid::new(),
        policy_binding: [7; 32],
    }
}

fn write_private(path: &Path, bytes: &[u8]) {
    let directory = Directory::open(path.parent().unwrap()).unwrap();
    let mut file = directory.create_file(path.file_name().unwrap()).unwrap();
    file.write_all(bytes).unwrap();
    file.sync_all().unwrap();
}

fn write_existing(path: &Path, bytes: &[u8]) {
    let mut file = OpenOptions::new()
        .write(true)
        .truncate(true)
        .open(path)
        .unwrap();
    file.write_all(bytes).unwrap();
    file.sync_all().unwrap();
}

fn stage_with_main(scratch: &Scratch) -> FreshStoreStageV1 {
    let target = scratch.target();
    let stage = StoreLease::acquire(&target)
        .unwrap()
        .begin_fresh_store_v1(&target, spec())
        .unwrap();
    write_existing(stage.database_path(), b"closed filesystem-only fixture");
    stage
}

#[test]
fn ntfs_publication_retains_full_identity_private_acl_and_lease() {
    let scratch = Scratch::new();
    let target = scratch.target();
    let stage = stage_with_main(&scratch);
    stage.stage_dir.require_private().unwrap();
    require_private_file(&stage.main).unwrap();
    require_private_file(&stage.intent).unwrap();
    let stage_path = stage.stage_path.clone();
    let identity = file_identity(&stage.main).unwrap();
    let published = stage.seal_closed_file().unwrap().publish().unwrap();
    assert_eq!(file_identity(&published.main).unwrap(), identity);
    assert!(!stage_path.exists());
    assert_eq!(
        fs::read(&target).unwrap(),
        b"closed filesystem-only fixture"
    );
    published.require_guards().unwrap();
    assert_eq!(
        StoreLease::acquire(&target).unwrap_err().kind(),
        io::ErrorKind::WouldBlock
    );
    drop(published);
    StoreLease::acquire(&target)
        .unwrap()
        .require_guards(&target)
        .unwrap();
}

#[test]
fn ntfs_no_clobber_preserves_the_competitor_and_can_retry_exact_stage() {
    let scratch = Scratch::new();
    let target = scratch.target();
    let prepared = stage_with_main(&scratch).seal_closed_file().unwrap();
    write_private(&target, b"competitor");
    let error = prepared.publish().unwrap_err();
    assert_eq!(error.kind(), io::ErrorKind::AlreadyExists);
    assert_eq!(fs::read(&target).unwrap(), b"competitor");
    let FreshStorePublishErrorV1::NotPublished { prepared, .. } = error else {
        panic!("preflight must retain exact retry capability");
    };
    fs::remove_file(&target).unwrap();
    prepared.publish().unwrap().require_guards().unwrap();
}

#[test]
fn ntfs_rename_errors_distinguish_retry_crossing_and_ambiguity() {
    for point in [
        FreshPublishFailPointV1::RenameErrorBeforeSyscall,
        FreshPublishFailPointV1::RenameErrorAfterSyscall,
        FreshPublishFailPointV1::RenameErrorAmbiguous,
    ] {
        let scratch = Scratch::new();
        let target = scratch.target();
        let prepared = stage_with_main(&scratch).seal_closed_file().unwrap();
        arm_publish_failpoint_for_test(point);
        let error = prepared.publish().unwrap_err();
        match point {
            FreshPublishFailPointV1::RenameErrorBeforeSyscall => {
                let FreshStorePublishErrorV1::NotPublished { prepared, .. } = error else {
                    panic!("pre-rename error lost its retry capability");
                };
                assert!(!target.exists());
                prepared.publish().unwrap().require_guards().unwrap();
            }
            FreshPublishFailPointV1::RenameErrorAfterSyscall => {
                let FreshStorePublishErrorV1::PublicationNeedsRecovery { recovery, .. } = error
                else {
                    panic!("crossed rename incorrectly reported unpublished");
                };
                assert_eq!(
                    recovery.phase(),
                    FreshStorePublicationRecoveryPhaseV1::RenameReportedErrorAfterCrossing
                );
                assert_eq!(
                    recovery.source_observation(),
                    FreshStoreNameObservationV1::Absent
                );
                assert_eq!(
                    recovery.target_observation(),
                    FreshStoreNameObservationV1::ExactRetainedMain
                );
                assert!(
                    recovery
                        .stage_path()
                        .unwrap()
                        .join(PREPARED_RECORD)
                        .exists()
                );
                recovery.require_guards().unwrap();
                assert_eq!(
                    StoreLease::acquire(&target).unwrap_err().kind(),
                    io::ErrorKind::WouldBlock
                );
            }
            FreshPublishFailPointV1::RenameErrorAmbiguous => {
                let FreshStorePublishErrorV1::PublicationNeedsRecovery { recovery, .. } = error
                else {
                    panic!("ambiguous rename incorrectly reported retryable");
                };
                assert_eq!(
                    recovery.phase(),
                    FreshStorePublicationRecoveryPhaseV1::RenameOutcomeAmbiguous
                );
                assert_eq!(
                    recovery.source_observation(),
                    FreshStoreNameObservationV1::ExactRetainedMain
                );
                assert_eq!(
                    recovery.target_observation(),
                    FreshStoreNameObservationV1::OtherOrUninspectable
                );
                assert_eq!(recovery.stage_path(), None);
                assert!(recovery.require_guards().is_err());
                assert_eq!(fs::read(&target).unwrap(), b"injected competing target");
            }
            _ => unreachable!(),
        }
    }
}

#[test]
fn ntfs_publication_failures_retain_phase_specific_authority() {
    for (point, expected) in [
        (
            FreshPublishFailPointV1::DurabilityBarrier,
            FreshStorePublicationRecoveryPhaseV1::DurabilityBarrierFailed,
        ),
        (
            FreshPublishFailPointV1::InitialGuard,
            FreshStorePublicationRecoveryPhaseV1::DurablePublicationInitialGuardFailed,
        ),
    ] {
        let scratch = Scratch::new();
        let prepared = stage_with_main(&scratch).seal_closed_file().unwrap();
        arm_publish_failpoint_for_test(point);
        let FreshStorePublishErrorV1::PublicationNeedsRecovery { recovery, .. } =
            prepared.publish().unwrap_err()
        else {
            panic!("pre-cleanup failure returned wrong capability");
        };
        assert_eq!(recovery.phase(), expected);
        assert!(recovery.stage_path().is_some());
        recovery.require_guards().unwrap();
    }
    for phase in [
        FreshStorePublishedResiduePhaseV1::VerifyIntentRecord,
        FreshStorePublishedResiduePhaseV1::RemoveIntentRecord,
        FreshStorePublishedResiduePhaseV1::SyncIntentRemoval,
        FreshStorePublishedResiduePhaseV1::VerifyPreparedRecord,
        FreshStorePublishedResiduePhaseV1::RemovePreparedRecord,
        FreshStorePublishedResiduePhaseV1::SyncPreparedRemoval,
        FreshStorePublishedResiduePhaseV1::RemoveStageDirectory,
        FreshStorePublishedResiduePhaseV1::SyncStageRemoval,
    ] {
        let scratch = Scratch::new();
        let prepared = stage_with_main(&scratch).seal_closed_file().unwrap();
        let stage_path = prepared.stage_path.clone();
        arm_publish_failpoint_for_test(FreshPublishFailPointV1::Cleanup(phase));
        let FreshStorePublishErrorV1::PublishedWithResidue { recovery, .. } =
            prepared.publish().unwrap_err()
        else {
            panic!("cleanup failure returned wrong capability");
        };
        assert_eq!(recovery.phase(), phase);
        let removed = phase == FreshStorePublishedResiduePhaseV1::SyncStageRemoval;
        assert_eq!(stage_path.exists(), !removed);
        assert_eq!(recovery.stage_path().is_none(), removed);
        recovery.require_guards().unwrap();
    }
    let scratch = Scratch::new();
    let prepared = stage_with_main(&scratch).seal_closed_file().unwrap();
    let stage_path = prepared.stage_path.clone();
    arm_publish_failpoint_for_test(FreshPublishFailPointV1::FinalGuard);
    let FreshStorePublishErrorV1::PublishedGuardNeedsRecovery { recovery, .. } =
        prepared.publish().unwrap_err()
    else {
        panic!("final guard failure retained fake stage evidence");
    };
    assert!(!stage_path.exists());
    recovery.require_guards().unwrap();
}

#[test]
fn ntfs_discard_is_explicit_bounded_and_rejects_unrecognized_or_hardlinked_members() {
    for prepared in [false, true] {
        let scratch = Scratch::new();
        let stage = stage_with_main(&scratch);
        let path = stage.stage_path.clone();
        let lease = if prepared {
            stage.seal_closed_file().unwrap().discard_closed().unwrap()
        } else {
            stage.discard_closed().unwrap()
        };
        assert!(!path.exists());
        lease.require_guards(&scratch.target()).unwrap();
    }
    let scratch = Scratch::new();
    let stage = stage_with_main(&scratch);
    let path = stage.stage_path.clone();
    write_private(&path.join("foreign"), b"preserve");
    assert!(stage.discard_closed().is_err());
    assert_eq!(fs::read(path.join("foreign")).unwrap(), b"preserve");

    let other = Scratch::new();
    let stage = stage_with_main(&other);
    let path = stage.stage_path.clone();
    write_private(&path.join("memory.db-journal"), b"earlier valid member");
    write_private(&path.join("memory.db-wal"), b"later invalid member");
    fs::hard_link(path.join("memory.db-wal"), other.0.join("wal-alias")).unwrap();
    assert!(stage.discard_closed().is_err());
    for name in [
        INTENT_RECORD,
        STAGE_DATABASE,
        "memory.db-journal",
        "memory.db-wal",
    ] {
        assert!(path.join(name).exists(), "partial deletion of {name}");
    }
}

#[test]
fn ntfs_drop_preserves_evidence_and_runtime_guard_permits_growth_and_sidecars() {
    let scratch = Scratch::new();
    let stage = stage_with_main(&scratch);
    let stage_path = stage.stage_path.clone();
    drop(stage);
    assert!(stage_path.join(INTENT_RECORD).exists());
    assert!(stage_path.join(STAGE_DATABASE).exists());
    let other = Scratch::new();
    let published = stage_with_main(&other)
        .seal_closed_file()
        .unwrap()
        .publish()
        .unwrap();
    write_existing(
        published.path(),
        b"SQLite may grow this database after publication",
    );
    write_private(&other.0.join("memory.db-wal"), b"runtime sidecar");
    assert!(published.require_guards().is_err());
    published.require_runtime_guards().unwrap();
}

#[test]
fn windows_records_bind_utf16_and_full_128_bit_file_ids_without_unix_wire_confusion() {
    let scratch = Scratch::new();
    let stage = stage_with_main(&scratch);
    let encoded = encode_intent(
        &stage.lease,
        &stage.target,
        stage.stage_identity,
        stage.spec,
    )
    .unwrap();
    assert!(encoded.starts_with(b"mneme.fresh-store-stage.intent.windows.v1\0"));
    let utf16: Vec<u8> = stage
        .target
        .as_os_str()
        .encode_wide()
        .flat_map(u16::to_le_bytes)
        .collect();
    assert!(encoded.ends_with(&utf16));
    let mut changed = stage.stage_identity;
    changed.id[15] ^= 1;
    assert_ne!(
        encoded,
        encode_intent(&stage.lease, &stage.target, changed, stage.spec).unwrap()
    );
    let encoded_main = encode_prepared(&encoded, stage.initial_main_identity).unwrap();
    assert!(encoded_main.starts_with(b"mneme.fresh-store-stage.prepared.windows.v1\0"));
    let mut changed_main = stage.initial_main_identity;
    changed_main.id[15] ^= 1;
    assert_ne!(
        encoded_main,
        encode_prepared(&encoded, changed_main).unwrap()
    );
    let too_long = PathBuf::from(OsString::from_wide(&vec![u16::from(b'x'); 2049]));
    assert!(require_target_binding_len(&too_long).is_err());
}

#[test]
fn ntfs_refuses_sidecars_empty_main_hardlinks_and_changed_records_without_publication() {
    let scratch = Scratch::new();
    let target = scratch.target();
    write_private(&scratch.0.join("memory.db-wal"), b"orphan");
    assert!(
        StoreLease::acquire(&target)
            .unwrap()
            .begin_fresh_store_v1(&target, spec())
            .is_err()
    );
    assert!(!fs::read_dir(&scratch.0).unwrap().any(|e| {
        e.unwrap()
            .file_name()
            .to_string_lossy()
            .starts_with(STAGE_PREFIX)
    }));
    for variant in [0, 1, 2] {
        let scratch = Scratch::new();
        let target = scratch.target();
        let stage = StoreLease::acquire(&target)
            .unwrap()
            .begin_fresh_store_v1(&target, spec())
            .unwrap();
        if variant == 1 {
            write_existing(stage.database_path(), b"main");
            fs::hard_link(stage.database_path(), scratch.0.join("alias")).unwrap();
        } else if variant == 2 {
            write_existing(stage.database_path(), b"main");
            write_existing(&stage.stage_path.join(INTENT_RECORD), b"forged intent");
        }
        assert!(stage.seal_closed_file().is_err());
        assert!(!target.exists());
    }
}

#[test]
fn ntfs_name_replacements_never_authorize_publication_or_impostor_cleanup() {
    let scratch = Scratch::new();
    let stage = stage_with_main(&scratch);
    let path = stage.database_path().to_path_buf();
    stage
        .stage_dir
        .remove_file(
            OsStr::new(STAGE_DATABASE),
            file_identity(&stage.main).unwrap(),
        )
        .unwrap();
    write_private(&path, b"impostor main");
    assert!(stage.seal_closed_file().is_err());
    assert_eq!(fs::read(&path).unwrap(), b"impostor main");
    assert!(!scratch.target().exists());

    let other = Scratch::new();
    let stage = stage_with_main(&other);
    let stage_path = stage.stage_path.clone();
    let detached = other.0.join("detached-stage");
    fs::rename(&stage_path, &detached).unwrap();
    drop(Directory::create(&stage_path).unwrap());
    write_private(&stage_path.join("foreign"), b"impostor directory");
    assert!(stage.discard_closed().is_err());
    assert_eq!(
        fs::read(stage_path.join("foreign")).unwrap(),
        b"impostor directory"
    );
    assert!(detached.join(INTENT_RECORD).exists());
    assert!(detached.join(STAGE_DATABASE).exists());
}

#[test]
fn ntfs_retained_main_and_guards_allow_sqlite_sharing_but_publication_requires_close() {
    use std::os::windows::fs::OpenOptionsExt;
    use windows_sys::Win32::Storage::FileSystem::{FILE_SHARE_READ, FILE_SHARE_WRITE};
    let scratch = Scratch::new();
    let stage = stage_with_main(&scratch);
    // Match sqlite3.c's Windows VFS rather than Rust's more permissive defaults.
    let sqlite_like = OpenOptions::new()
        .read(true)
        .write(true)
        .share_mode(FILE_SHARE_READ | FILE_SHARE_WRITE)
        .open(stage.database_path())
        .unwrap();
    let prepared = stage.seal_closed_file().unwrap();
    let FreshStorePublishErrorV1::NotPublished { prepared, .. } = prepared.publish().unwrap_err()
    else {
        panic!("open database must refuse publication before crossing rename");
    };
    drop(sqlite_like);
    let published = prepared.publish().unwrap();
    let _sqlite_runtime = OpenOptions::new()
        .read(true)
        .write(true)
        .share_mode(FILE_SHARE_READ | FILE_SHARE_WRITE)
        .open(published.path())
        .unwrap();
    published.require_runtime_guards().unwrap();
}
