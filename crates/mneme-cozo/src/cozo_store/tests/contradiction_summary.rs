use super::*;
use mneme_core::ports::{CONTRADICTION_SUMMARY_TIMEOUT, ContradictionSummaryRequest};

#[tokio::test]
async fn contradiction_summary_preserves_managed_cozo_epoch() {
    let store = test_managed_v1_store_with_row();
    let a = active_node(NodeId(Ulid::from(74_100u128)));
    let b = active_node(NodeId(Ulid::from(74_101u128)));
    store.put_node(&a).await.unwrap();
    store.put_node(&b).await.unwrap();
    store
        .observe_contradiction(a.id(), b.id(), 10)
        .await
        .unwrap();
    let before = store.storage_contract_generation().unwrap();
    let request = ContradictionSummaryRequest::new(2, CONTRADICTION_SUMMARY_TIMEOUT).unwrap();
    let result = store
        .contradiction_summary(&request)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(result.open_found(), 1);
    assert!(result.complete());
    assert_eq!(store.storage_contract_generation().unwrap(), before);
}

#[tokio::test]
async fn contradiction_summary_cozo_snapshot_lock_wait_is_deadline_bound() {
    let store = CozoStore::new(2).unwrap();
    let held = store.db.multi_transaction(true);
    held.run_script("?[value] <- [[1]]", BTreeMap::new())
        .unwrap();
    let request = ContradictionSummaryRequest::new(2, Duration::from_millis(10)).unwrap();
    let started = std::time::Instant::now();
    assert!(store.contradiction_summary(&request).await.is_err());
    assert!(started.elapsed() < Duration::from_secs(1));
    assert_eq!(store.backend_activity.in_flight(), 0);
    held.abort().unwrap();
    assert!(
        store
            .contradiction_summary(&request)
            .await
            .unwrap()
            .unwrap()
            .complete()
    );
}

#[tokio::test]
async fn contradiction_summary_rejects_corrupt_physical_rows_instead_of_counting_them() {
    let store = CozoStore::new(2).unwrap();
    let a = active_node(NodeId(Ulid::from(74_100u128)));
    let b = active_node(NodeId(Ulid::from(74_101u128)));
    store.put_node(&a).await.unwrap();
    store.put_node(&b).await.unwrap();
    store
        .observe_contradiction(a.id(), b.id(), 10)
        .await
        .unwrap();
    store
        .run(
            "?[lo, hi, observations, first_seen, last_seen, resolution] := \
         *contradiction{lo, hi, observations, first_seen, last_seen}, resolution = 'unknown' \
         :put contradiction {lo, hi => observations, first_seen, last_seen, resolution}",
            BTreeMap::new(),
            true,
        )
        .unwrap();
    let request = ContradictionSummaryRequest::new(2, CONTRADICTION_SUMMARY_TIMEOUT).unwrap();
    assert!(store.contradiction_summary(&request).await.is_err());
    assert_eq!(store.backend_activity.in_flight(), 0);
}

#[tokio::test]
async fn contradiction_summary_sqlite_read_and_contended_snapshot_are_read_only() {
    let directory =
        std::env::temp_dir().join(format!("mneme-contradiction-summary-{}", Ulid::new()));
    std::fs::create_dir(&directory).unwrap();
    let path = directory.join("memory.db");
    let store = CozoStore::open(path.to_str().unwrap(), 2).unwrap();
    let a = active_node(NodeId(Ulid::from(74_100u128)));
    let b = active_node(NodeId(Ulid::from(74_101u128)));
    store.put_node(&a).await.unwrap();
    store.put_node(&b).await.unwrap();
    store
        .observe_contradiction(a.id(), b.id(), 10)
        .await
        .unwrap();
    let before = serde_json::to_value(store.export().await.unwrap()).unwrap();
    let request = ContradictionSummaryRequest::new(2, CONTRADICTION_SUMMARY_TIMEOUT).unwrap();
    let result = store
        .contradiction_summary(&request)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(result.open_found(), 1);
    assert!(result.complete());
    assert_eq!(
        serde_json::to_value(store.export().await.unwrap()).unwrap(),
        before
    );

    let held = store.db.multi_transaction(true);
    held.run_script("?[value] <- [[1]]", BTreeMap::new())
        .unwrap();
    let request = ContradictionSummaryRequest::new(2, Duration::from_millis(10)).unwrap();
    let started = std::time::Instant::now();
    assert!(store.contradiction_summary(&request).await.is_err());
    assert!(started.elapsed() < Duration::from_secs(1));
    assert_eq!(store.backend_activity.in_flight(), 0);
    held.abort().unwrap();
    assert_eq!(
        serde_json::to_value(store.export().await.unwrap()).unwrap(),
        before
    );
    drop(store);
    std::fs::remove_dir_all(directory).unwrap();
}
