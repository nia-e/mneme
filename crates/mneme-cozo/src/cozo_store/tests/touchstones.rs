use super::*;
use crate::touchstone_tests::{authored, capture, input, target};

#[tokio::test]
async fn touchstone_record_and_reverse_insert_failures_roll_back_entire_capture() {
    for step in [6, 7] {
        let store = CozoStore::new(4).unwrap();
        let scene = target(NodeId(Ulid::new()), "the original bounded scene");
        store.put_node(&scene).await.unwrap();
        let input = input(store.db_id(), &scene);
        let (owner, proof) = authored(&format!("rollback-{step}"), &input);
        super::super::capture::arm_capture_failure(owner.id(), step);
        assert!(capture(&store, &owner, &proof, &input).await.is_err());
        assert!(store.get_node(owner.id()).await.unwrap().is_none());
        assert!(store.get_touchstone(owner.id()).await.unwrap().is_none());
        assert!(
            store
                .touchstone_referrers_page(
                    &TouchstoneReferrersRequest::new(scene.id(), None, 32).unwrap()
                )
                .await
                .unwrap()
                .items
                .is_empty()
        );
        let actual = store.export().await.unwrap();
        assert!(actual.touchstones.is_empty());
        assert!(actual.vectors.is_empty());
        assert_eq!(actual.nodes.len(), 1);
        assert_eq!(
            capture(&store, &owner, &proof, &input).await.unwrap(),
            mneme_core::ports::CaptureCommitOutcome::Applied
        );
        assert_eq!(store.export().await.unwrap().touchstones.len(), 1);
    }
}

#[tokio::test]
async fn native_touchstone_import_export_reopen_preserves_deleted_target_snapshot() {
    let source = crate::MemStore::new(4);
    let scene = target(NodeId(Ulid::new()), "the exact historical summary");
    source.put_node(&scene).await.unwrap();
    let input = input(source.db_id(), &scene);
    let (owner, proof) = authored("native-reopen", &input);
    capture(&source, &owner, &proof, &input).await.unwrap();
    source.delete_node(scene.id()).await.unwrap();
    let expected = source.export();
    let root = std::env::temp_dir().join(format!("mneme-touchstone-reopen-{}", Ulid::new()));
    std::fs::create_dir(&root).unwrap();
    let root = std::fs::canonicalize(root).unwrap();
    let path = root.join("store.db");
    CozoStore::materialize_fresh_current(&path, Ulid::new(), &source)
        .await
        .unwrap();
    let current = reopen_current_test_store(&path).unwrap();
    assert_eq!(current.db_id(), source.db_id());
    current.verify_import(&expected).await.unwrap();
    assert_eq!(
        capture(&current, &owner, &proof, &input).await.unwrap(),
        mneme_core::ports::CaptureCommitOutcome::AlreadyApplied
    );
    current.prepare_for_file_move().unwrap();
    drop(current);
    let reopened = reopen_current_test_store(&path).unwrap();
    reopened.verify_import(&expected).await.unwrap();
    reopened.prepare_for_file_move().unwrap();
    drop(reopened);
    std::fs::remove_dir_all(root).unwrap();
}

#[tokio::test]
async fn native_target_merge_preserves_snapshot_and_reverse_identity() {
    let store = CozoStore::new(4).unwrap();
    let scene = target(NodeId(Ulid::new()), "the source that mattered");
    let replacement = target(NodeId(Ulid::new()), "a later reusable semantic note");
    store.put_node(&scene).await.unwrap();
    store.put_node(&replacement).await.unwrap();
    let input = input(store.db_id(), &scene);
    let (owner, proof) = authored("target-merge", &input);
    capture(&store, &owner, &proof, &input).await.unwrap();
    let record = store.get_touchstone(owner.id()).await.unwrap().unwrap();
    store
        .observe_merge_candidate(replacement.id(), scene.id(), 3)
        .await
        .unwrap();
    store
        .commit_full_merge(&FullMergeCommit::new(replacement.id(), scene.id(), 4).unwrap())
        .await
        .unwrap();
    assert_eq!(
        store.get_touchstone(owner.id()).await.unwrap().unwrap(),
        record
    );
    assert_eq!(
        store
            .touchstone_referrers_page(
                &TouchstoneReferrersRequest::new(scene.id(), None, 32).unwrap()
            )
            .await
            .unwrap()
            .items[0]
            .id,
        owner.id()
    );
    assert!(
        store
            .touchstone_referrers_page(
                &TouchstoneReferrersRequest::new(replacement.id(), None, 32).unwrap()
            )
            .await
            .unwrap()
            .items
            .is_empty()
    );
}
