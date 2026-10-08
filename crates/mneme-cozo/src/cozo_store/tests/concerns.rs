use super::*;
use crate::concern_tests::{finding, node, notice, resulting};
use mneme_core::concern::*;

#[tokio::test]
async fn concern_cas_lifecycle_parity() {
    crate::concern_tests::cas_lifecycle(&CozoStore::new(1).unwrap()).await;
}
#[tokio::test]
async fn concern_indexed_endpoint_page_parity() {
    crate::concern_tests::paging(&CozoStore::new(1).unwrap()).await;
}

#[tokio::test]
async fn concern_competing_full_row_cas_has_one_winner() {
    let store = std::sync::Arc::new(CozoStore::new(1).unwrap());
    let a = node(1, "a");
    let b = node(2, "b");
    store.put_node(&a).await.unwrap();
    store.put_node(&b).await.unwrap();
    let initial = resulting(
        store
            .update_concern(&notice(&a, &b, ConcernKind::Disagreement))
            .await
            .unwrap(),
    );
    let mut jobs = Vec::new();
    for scope in ["left", "right"] {
        let store = store.clone();
        let update = finding(initial.clone(), scope);
        jobs.push(tokio::spawn(async move {
            store.update_concern(&update).await.unwrap()
        }));
    }
    let mut applied = 0;
    let mut refused = 0;
    for job in jobs {
        match job.await.unwrap() {
            ConcernCommitOutcome::Applied { .. } => applied += 1,
            ConcernCommitOutcome::Refused {
                reason: ConcernRefusal::StaleRow,
                ..
            } => refused += 1,
            other => panic!("unexpected {other:?}"),
        }
    }
    assert_eq!((applied, refused), (1, 1));
}
#[tokio::test]
async fn concern_stale_import_reopen_and_malformed_refusal() {
    let source = crate::MemStore::new(1);
    let a = node(1, "a");
    let b = node(2, "b");
    source.put_node(&a).await.unwrap();
    source.put_node(&b).await.unwrap();
    source
        .update_concern(&notice(&a, &b, ConcernKind::Disagreement))
        .await
        .unwrap();
    source.put_node(&node(2, "changed")).await.unwrap();
    let root = std::fs::canonicalize(std::env::temp_dir())
        .unwrap()
        .join(format!("mneme-concerns-{}", Ulid::new()));
    std::fs::create_dir(&root).unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&root, std::fs::Permissions::from_mode(0o700)).unwrap();
    }
    let path = root.join("memory.db");
    let mut store = CozoStore::open(path.to_str().unwrap(), 1).unwrap();
    store.import_mem(&source).await.unwrap();
    assert_eq!(
        store.export().await.unwrap().concerns,
        source.export().concerns
    );
    drop(store);
    let store = reopen_current_test_store(&path).unwrap();
    store.verify_import(&source.export()).await.unwrap();
    let key = source.export().concerns[0].binding().key();
    let p = BTreeMap::from([
        ("lo".into(), dv_str(&key.endpoints()[0].0.to_string())),
        ("hi".into(), dv_str(&key.endpoints()[1].0.to_string())),
    ]);
    store
        .run(
            "?[lo,hi,kind,data] <- [[$lo,$hi,'disagreement','{}']] :put concern {lo,hi,kind=>data}",
            p,
            true,
        )
        .unwrap();
    assert!(store.get_concern(&key).await.is_err());
    assert!(store.export().await.is_err());
    drop(store);
    std::fs::remove_dir_all(root).unwrap();
}
#[tokio::test]
async fn concern_failed_node_delete_rolls_back_rows() {
    let store = CozoStore::new(1).unwrap();
    let a = node(1, "a");
    let b = node(2, "b");
    store.put_node(&a).await.unwrap();
    store.put_node(&b).await.unwrap();
    let row = resulting(
        store
            .update_concern(&notice(&a, &b, ConcernKind::Disagreement))
            .await
            .unwrap(),
    );
    let tx = store.db.multi_transaction(true);
    let p = BTreeMap::from([("id".into(), dv_str(&a.id().0.to_string()))]);
    tx_run(
        &tx,
        "?[lo,hi,kind] := lo=$id, *concern{lo:$id,hi,kind} :rm concern {lo,hi,kind}",
        p,
    )
    .unwrap();
    assert!(tx_run(&tx, "?[x] := *nonexistent{x}", BTreeMap::new()).is_err());
    let _ = tx.abort();
    assert_eq!(
        store.get_concern(&row.binding().key()).await.unwrap(),
        Some(row)
    );
}

#[tokio::test]
async fn concern_degree_exceeds_graph_structural_ceiling() {
    crate::concern_tests::unbounded_degree(&CozoStore::new(1).unwrap()).await;
}
#[tokio::test]
async fn concern_body_reference_and_maximum_escaped_bounds() {
    crate::concern_tests::body_ref_and_escaped_bounds(&CozoStore::new(1).unwrap()).await;
}
