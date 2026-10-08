use super::*;
use std::fs::{self, OpenOptions};
use std::io::Write;
use std::os::unix::fs::{MetadataExt, OpenOptionsExt, PermissionsExt, symlink};

struct Scratch(PathBuf);

impl Scratch {
    fn new() -> Self {
        let path = std::env::temp_dir().join(format!("mneme-fresh-stage-{}", Ulid::new()));
        fs::create_dir(&path).unwrap();
        fs::set_permissions(&path, fs::Permissions::from_mode(0o700)).unwrap();
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
    let mut file = OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(path)
        .unwrap();
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
    let lease = StoreLease::acquire(&target).unwrap();
    let stage = lease.begin_fresh_store_v1(&target, spec()).unwrap();
    write_existing(stage.database_path(), b"closed filesystem-only fixture");
    stage
}

#[test]
fn stage_is_private_durable_and_retains_the_target_lease() {
    let scratch = Scratch::new();
    let target = scratch.target();
    let lease = StoreLease::acquire(&target).unwrap();
    let stage = lease.begin_fresh_store_v1(&target, spec()).unwrap();

    assert!(!target.exists());
    assert_eq!(
        fs::metadata(stage.database_path().parent().unwrap())
            .unwrap()
            .permissions()
            .mode()
            & 0o7777,
        0o700
    );
    let intent = stage.database_path().parent().unwrap().join(INTENT_RECORD);
    assert_eq!(
        fs::metadata(intent).unwrap().permissions().mode() & 0o7777,
        0o600
    );
    assert_eq!(
        fs::metadata(stage.database_path())
            .unwrap()
            .permissions()
            .mode()
            & 0o7777,
        0o600
    );
    assert_eq!(fs::metadata(stage.database_path()).unwrap().len(), 0);
    assert_eq!(
        StoreLease::acquire(&target).unwrap_err().kind(),
        io::ErrorKind::WouldBlock
    );
}

#[test]
fn both_new_and_existing_lease_paths_sync_before_intent_binding() {
    for existing in [false, true] {
        let scratch = Scratch::new();
        let target = scratch.target();
        if existing {
            drop(StoreLease::acquire(&target).expect("seed existing lease inode"));
        }

        let operation = spec();
        let stage_path = scratch.0.join(stage_name(operation.operation_id));
        crate::arm_lease_sync_failure_for_test();
        let lease = StoreLease::acquire(&target)
            .expect("ordinary lease acquisition must not consume the fresh-intent sync hook");
        let error = lease
            .begin_fresh_store_v1(&target, operation)
            .expect_err("fresh intent must cross the retained-lease durability cut");
        assert!(
            error
                .to_string()
                .contains("injected retained lease durability failure"),
            "{} lease path skipped the fresh-intent durability cut: {error}",
            if existing {
                "pre-existing"
            } else {
                "create-new"
            }
        );
        assert!(
            !stage_path.exists(),
            "lease sync failure created fresh operation evidence"
        );
    }
}

#[test]
fn drop_preserves_the_operation_directory() {
    let scratch = Scratch::new();
    let target = scratch.target();
    let lease = StoreLease::acquire(&target).unwrap();
    let stage = lease.begin_fresh_store_v1(&target, spec()).unwrap();
    let stage_path = stage.database_path().parent().unwrap().to_path_buf();
    drop(stage);
    assert!(
        stage_path.exists(),
        "Drop must not destroy recovery evidence"
    );
    StoreLease::acquire(&target).unwrap();
}

#[test]
fn seal_and_publish_preserve_inode_and_never_release_lease() {
    let scratch = Scratch::new();
    let target = scratch.target();
    let stage = stage_with_main(&scratch);
    let stage_path = stage.database_path().parent().unwrap().to_path_buf();
    let staged_metadata = fs::metadata(stage.database_path()).unwrap();
    let prepared = stage.seal_closed_file().unwrap();
    let published = prepared.publish().unwrap();

    assert_eq!(published.path(), target);
    assert_eq!(fs::metadata(&target).unwrap().ino(), staged_metadata.ino());
    published.require_guards().unwrap();
    assert_eq!(
        StoreLease::acquire(&target).unwrap_err().kind(),
        io::ErrorKind::WouldBlock
    );
    assert!(!stage_path.exists());
    let path = published.path().to_path_buf();
    assert_eq!(
        StoreLease::acquire(&path).unwrap_err().kind(),
        io::ErrorKind::WouldBlock
    );
    drop(published);
    StoreLease::acquire(&path).unwrap();
}

#[test]
fn runtime_guards_retain_main_identity_but_permit_sqlite_growth_and_sidecars() {
    let scratch = Scratch::new();
    let target = scratch.target();
    let published = stage_with_main(&scratch)
        .seal_closed_file()
        .unwrap()
        .publish()
        .unwrap();

    write_existing(
        &target,
        b"a live sqlite runtime may grow the retained published inode",
    );
    let mut wal = target.as_os_str().to_os_string();
    wal.push("-wal");
    write_private(Path::new(&wal), b"runtime-owned sqlite sidecar");

    assert!(
        published.require_guards().is_err(),
        "the pre-open guard must continue to require a sidecar-free artifact"
    );
    published
        .require_runtime_guards()
        .expect("runtime guard must retain lease and stable main identity without freezing bytes");
    assert_eq!(
        StoreLease::acquire(&target).unwrap_err().kind(),
        io::ErrorKind::WouldBlock
    );
}

#[test]
fn no_clobber_race_preserves_competing_target_and_prepared_stage() {
    let scratch = Scratch::new();
    let target = scratch.target();
    let stage = stage_with_main(&scratch);
    let prepared = stage.seal_closed_file().unwrap();
    let stage_path = prepared.database_path().parent().unwrap().to_path_buf();
    write_private(&target, b"competing target");

    let error = prepared.publish().unwrap_err();
    assert_eq!(error.kind(), io::ErrorKind::AlreadyExists);
    assert_eq!(fs::read(&target).unwrap(), b"competing target");
    assert!(stage_path.join(STAGE_DATABASE).exists());

    let prepared = match error {
        FreshStorePublishErrorV1::NotPublished {
            source: _,
            prepared,
        } => prepared,
        other => panic!("no-clobber preflight returned the wrong phase: {other:?}"),
    };
    fs::remove_file(&target).unwrap();
    let published = (*prepared).publish().unwrap();
    published.require_guards().unwrap();
}

#[test]
fn rename_error_exact_source_and_absent_target_is_the_only_retryable_state() {
    let scratch = Scratch::new();
    let target = scratch.target();
    let prepared = stage_with_main(&scratch).seal_closed_file().unwrap();
    let stage_path = prepared.database_path().parent().unwrap().to_path_buf();

    arm_publish_failpoint_for_test(FreshPublishFailPointV1::RenameErrorBeforeSyscall);
    let error = prepared.publish().unwrap_err();
    let prepared = match error {
        FreshStorePublishErrorV1::NotPublished { prepared, .. } => prepared,
        other => panic!("proved unpublished state was not retryable: {other:?}"),
    };
    assert!(!target.exists());
    assert!(stage_path.join(STAGE_DATABASE).exists());

    let published = prepared.publish().unwrap();
    published.require_guards().unwrap();
}

#[test]
fn rename_error_exact_target_and_absent_source_is_crossed_recovery() {
    let scratch = Scratch::new();
    let target = scratch.target();
    let prepared = stage_with_main(&scratch).seal_closed_file().unwrap();
    let stage_path = prepared.database_path().parent().unwrap().to_path_buf();

    arm_publish_failpoint_for_test(FreshPublishFailPointV1::RenameErrorAfterSyscall);
    let error = prepared.publish().unwrap_err();
    assert_eq!(error.kind(), io::ErrorKind::Other);
    let recovery = match error {
        FreshStorePublishErrorV1::PublicationNeedsRecovery { recovery, .. } => recovery,
        other => panic!("crossed rename error returned the wrong phase: {other:?}"),
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
    assert_eq!(recovery.path(), target);
    assert_eq!(recovery.stage_path(), Some(stage_path.as_path()));
    assert!(target.exists());
    assert!(!stage_path.join(STAGE_DATABASE).exists());
    assert!(stage_path.join(INTENT_RECORD).exists());
    assert!(stage_path.join(PREPARED_RECORD).exists());
    recovery.require_guards().unwrap();
    assert_eq!(
        StoreLease::acquire(&target).unwrap_err().kind(),
        io::ErrorKind::WouldBlock
    );
    drop(recovery);
    assert!(
        target.exists(),
        "Drop must not pretend to recover publication"
    );
}

#[test]
fn rename_error_without_exact_source_absent_target_is_ambiguous_without_a_stage_recipe() {
    let scratch = Scratch::new();
    let target = scratch.target();
    let prepared = stage_with_main(&scratch).seal_closed_file().unwrap();

    arm_publish_failpoint_for_test(FreshPublishFailPointV1::RenameErrorAmbiguous);
    let error = prepared.publish().unwrap_err();
    assert_eq!(error.kind(), io::ErrorKind::AlreadyExists);
    let recovery = match error {
        FreshStorePublishErrorV1::PublicationNeedsRecovery { recovery, .. } => recovery,
        other => panic!("ambiguous rename error returned the wrong phase: {other:?}"),
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
    assert_eq!(fs::read(&target).unwrap(), b"injected competing target");
    assert!(recovery.require_guards().is_err());
    assert_eq!(
        StoreLease::acquire(&target).unwrap_err().kind(),
        io::ErrorKind::WouldBlock
    );
}

#[test]
fn crossed_but_not_durable_and_durable_but_unguarded_are_distinct() {
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
        let error = prepared.publish().unwrap_err();
        let recovery = match error {
            FreshStorePublishErrorV1::PublicationNeedsRecovery { recovery, .. } => recovery,
            other => panic!("pre-cleanup failure returned the wrong phase: {other:?}"),
        };
        assert_eq!(recovery.phase(), expected);
        assert_eq!(
            recovery.source_observation(),
            FreshStoreNameObservationV1::Absent
        );
        assert_eq!(
            recovery.target_observation(),
            FreshStoreNameObservationV1::ExactRetainedMain
        );
        assert!(recovery.stage_path().is_some());
        recovery.require_guards().unwrap();
    }
}

#[test]
fn cleanup_failure_is_durable_publication_with_phase_tagged_residue() {
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
        let target = scratch.target();
        let prepared = stage_with_main(&scratch).seal_closed_file().unwrap();
        let stage_path = prepared.database_path().parent().unwrap().to_path_buf();

        arm_publish_failpoint_for_test(FreshPublishFailPointV1::Cleanup(phase));
        let error = prepared.publish().unwrap_err();
        let recovery = match error {
            FreshStorePublishErrorV1::PublishedWithResidue { recovery, .. } => recovery,
            other => panic!("cleanup failure masqueraded as another phase: {other:?}"),
        };
        assert_eq!(recovery.phase(), phase);
        assert_eq!(recovery.path(), target);
        if phase == FreshStorePublishedResiduePhaseV1::SyncStageRemoval {
            assert_eq!(recovery.stage_path(), None);
            assert!(!stage_path.exists());
        } else {
            assert_eq!(recovery.stage_path(), Some(stage_path.as_path()));
            assert!(stage_path.exists());
        }
        recovery.require_guards().unwrap();
    }
}

#[test]
fn final_guard_failure_retains_only_the_published_capability() {
    let scratch = Scratch::new();
    let target = scratch.target();
    let prepared = stage_with_main(&scratch).seal_closed_file().unwrap();
    let stage_path = prepared.database_path().parent().unwrap().to_path_buf();

    arm_publish_failpoint_for_test(FreshPublishFailPointV1::FinalGuard);
    let error = prepared.publish().unwrap_err();
    let recovery = match error {
        FreshStorePublishErrorV1::PublishedGuardNeedsRecovery { recovery, .. } => recovery,
        other => panic!("final guard failure retained fake stage state: {other:?}"),
    };
    assert_eq!(recovery.path(), target);
    assert!(!stage_path.exists());
    recovery.require_guards().unwrap();
    assert_eq!(
        StoreLease::acquire(&target).unwrap_err().kind(),
        io::ErrorKind::WouldBlock
    );
}

#[test]
fn explicit_discard_is_identity_bound_and_drop_is_not_cleanup() {
    let scratch = Scratch::new();
    let stage = stage_with_main(&scratch);
    let stage_path = stage.database_path().parent().unwrap().to_path_buf();
    let foreign = stage_path.join("foreign");
    write_private(&foreign, b"preserve me");
    assert!(stage.discard_closed().is_err());
    assert!(foreign.exists());
    assert!(stage_path.exists());
}

#[test]
fn explicit_discard_removes_only_the_exact_owned_inventory() {
    let scratch = Scratch::new();
    let target = scratch.target();
    let stage = stage_with_main(&scratch);
    let stage_path = stage.database_path().parent().unwrap().to_path_buf();
    write_private(&stage_path.join("memory.db-wal"), b"unpublished wal");
    let lease = stage.discard_closed().unwrap();
    assert!(!stage_path.exists());
    assert!(!target.exists());
    lease.require_guards(&target).unwrap();
}

#[test]
fn discard_prevalidates_every_member_before_deleting_anything() {
    let scratch = Scratch::new();
    let stage = stage_with_main(&scratch);
    let stage_path = stage.database_path().parent().unwrap().to_path_buf();
    let journal = stage_path.join("memory.db-journal");
    let malformed_later_wal = stage_path.join("memory.db-wal");
    write_private(&journal, b"valid earlier sidecar");
    write_private(&malformed_later_wal, b"malformed later sidecar");
    fs::set_permissions(&malformed_later_wal, fs::Permissions::from_mode(0o644)).unwrap();

    assert!(stage.discard_closed().is_err());
    for path in [
        stage_path.join(INTENT_RECORD),
        stage_path.join(STAGE_DATABASE),
        journal,
        malformed_later_wal,
    ] {
        assert!(
            path.exists(),
            "discard partially deleted {}",
            path.display()
        );
    }
}

#[test]
fn explicit_discard_of_prepared_stage_removes_its_records_and_main() {
    let scratch = Scratch::new();
    let target = scratch.target();
    let stage = stage_with_main(&scratch);
    let stage_path = stage.database_path().parent().unwrap().to_path_buf();
    let prepared = stage.seal_closed_file().unwrap();
    let lease = prepared.discard_closed().unwrap();

    assert!(!stage_path.exists());
    assert!(!target.exists());
    lease.require_guards(&target).unwrap();
}

#[test]
fn wrong_lease_relative_target_and_existing_sidecars_refuse_without_stage() {
    let scratch = Scratch::new();
    let target = scratch.target();
    let operation = spec();
    let sibling = scratch.0.join("sibling.db");
    let wrong = StoreLease::acquire(&sibling).unwrap();
    assert!(wrong.begin_fresh_store_v1(&target, operation).is_err());

    let relative = Path::new("memory.db");
    let lease = StoreLease::acquire(&target).unwrap();
    assert!(lease.begin_fresh_store_v1(relative, operation).is_err());

    write_private(&target.with_file_name("memory.db-wal"), b"orphan wal");
    let lease = StoreLease::acquire(&target).unwrap();
    assert!(lease.begin_fresh_store_v1(&target, operation).is_err());
    assert!(fs::read_dir(&scratch.0).unwrap().all(|entry| {
        !entry
            .unwrap()
            .file_name()
            .to_string_lossy()
            .starts_with(STAGE_PREFIX)
    }));
}

#[test]
fn fresh_stage_refuses_root_owned_sticky_or_shared_parent_before_mkdir() {
    let parent = fs::canonicalize("/private/tmp")
        .or_else(|_| fs::canonicalize("/tmp"))
        .unwrap();
    let metadata = fs::metadata(&parent).unwrap();
    if metadata.uid() != 0 || metadata.permissions().mode() & 0o7777 != 0o1777 {
        return;
    }

    let operation = spec();
    let target = parent.join(format!("mneme-fresh-shared-parent-{}.db", Ulid::new()));
    let stage_path = parent.join(stage_name(operation.operation_id));
    let lock = crate::store_lock_path(&target).unwrap();
    let lease = StoreLease::acquire(&target).unwrap();
    let error = lease.begin_fresh_store_v1(&target, operation).unwrap_err();
    assert_eq!(error.kind(), io::ErrorKind::PermissionDenied);
    assert!(error.to_string().contains("shared or root-owned sticky"));
    assert!(!stage_path.exists());
    fs::remove_file(lock).unwrap();
}

#[test]
fn symlink_main_and_hardlinked_main_refuse_without_publication() {
    let scratch = Scratch::new();
    let target = scratch.target();
    let lease = StoreLease::acquire(&target).unwrap();
    let stage = lease.begin_fresh_store_v1(&target, spec()).unwrap();
    let victim = scratch.0.join("victim");
    write_private(&victim, b"victim");
    fs::remove_file(stage.database_path()).unwrap();
    symlink(&victim, stage.database_path()).unwrap();
    assert!(stage.seal_closed_file().is_err());
    assert_eq!(fs::read(&victim).unwrap(), b"victim");

    let other = Scratch::new();
    let target = other.target();
    let lease = StoreLease::acquire(&target).unwrap();
    let stage = lease.begin_fresh_store_v1(&target, spec()).unwrap();
    write_existing(stage.database_path(), b"main");
    fs::hard_link(stage.database_path(), other.0.join("alias")).unwrap();
    assert!(stage.seal_closed_file().is_err());
    assert!(!target.exists());
}

#[test]
fn restrictive_umask_cannot_weaken_or_poison_stage_modes() {
    const CHILD: &str = "MNEME_FRESH_STAGE_UMASK_CHILD";
    if std::env::var_os(CHILD).is_some() {
        let scratch = Scratch::new();
        let target = scratch.target();
        let old = unsafe { libc::umask(0o777) };
        let lease = StoreLease::acquire(&target).unwrap();
        let stage = lease.begin_fresh_store_v1(&target, spec()).unwrap();
        assert_eq!(
            fs::metadata(stage.database_path().parent().unwrap())
                .unwrap()
                .permissions()
                .mode()
                & 0o7777,
            0o700
        );
        assert_eq!(
            fs::metadata(stage.database_path().parent().unwrap().join(INTENT_RECORD))
                .unwrap()
                .permissions()
                .mode()
                & 0o7777,
            0o600
        );
        assert_eq!(
            fs::metadata(stage.database_path())
                .unwrap()
                .permissions()
                .mode()
                & 0o7777,
            0o600
        );
        unsafe { libc::umask(old) };
        return;
    }
    let output = std::process::Command::new(std::env::current_exe().unwrap())
        .args([
            "--exact",
            "fresh::tests::restrictive_umask_cannot_weaken_or_poison_stage_modes",
        ])
        .env(CHILD, "1")
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "umask child failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
}

#[test]
fn nil_operation_zero_binding_and_stage_name_target_are_rejected() {
    let scratch = Scratch::new();
    let target = scratch.target();
    let lease = StoreLease::acquire(&target).unwrap();
    assert!(
        lease
            .begin_fresh_store_v1(
                &target,
                FreshStoreStageSpecV1 {
                    operation_id: Ulid::nil(),
                    policy_binding: [7; 32],
                },
            )
            .is_err()
    );

    let lease = StoreLease::acquire(&target).unwrap();
    assert!(
        lease
            .begin_fresh_store_v1(
                &target,
                FreshStoreStageSpecV1 {
                    operation_id: Ulid::new(),
                    policy_binding: [0; 32],
                },
            )
            .is_err()
    );

    let operation = spec();
    let target = scratch.0.join(stage_name(operation.operation_id));
    let lease = StoreLease::acquire(&target).unwrap();
    assert!(lease.begin_fresh_store_v1(&target, operation).is_err());
    assert!(!target.exists());
}

#[test]
fn ascii_casefolded_stage_name_collision_is_rejected_before_mkdir() {
    let scratch = Scratch::new();
    let operation = spec();
    let exact_stage_name = stage_name(operation.operation_id);
    let folded_target_name = exact_stage_name.to_string_lossy().to_ascii_uppercase();
    let target = scratch.0.join(folded_target_name);
    let lease = StoreLease::acquire(&target).unwrap();

    let error = lease.begin_fresh_store_v1(&target, operation).unwrap_err();
    assert_eq!(error.kind(), io::ErrorKind::InvalidInput);
    assert!(error.to_string().contains("ASCII case-folding"));
    assert!(!scratch.0.join(exact_stage_name).exists());
}

#[test]
fn target_binding_length_has_a_fixed_preflight_bound() {
    use std::os::unix::ffi::OsStringExt;

    let exact = PathBuf::from(OsString::from_vec(vec![b'x'; 4096]));
    record::require_target_binding_len(&exact).unwrap();
    let too_long = PathBuf::from(OsString::from_vec(vec![b'x'; 4097]));
    let error = record::require_target_binding_len(&too_long).unwrap_err();
    assert_eq!(error.kind(), io::ErrorKind::InvalidInput);
}
