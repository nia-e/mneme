//! Exercises the cozo-backed adapter directly at the port level. Only built when
//! the `cozo` feature is on: `cargo test -p mneme-cozo --features cozo`.
#![cfg(feature = "cozo")]

use mneme_core::ports::{
    Budget, ColdPath, EmbeddingFingerprintInit, EmbeddingMetadataStore, Error, FullMergeCommit,
    FullMergeCommitOutcome, GraphStore, LexicalIndex, Scored, StatusFilter, SupersedeCommit,
    SupersedeCommitOutcome, TaggedAnnBatch, TaggedAnnLaneRequest, TaggedAnnRequest,
    TaggedAnnWorkLimits, TaggedSeedCoverage, Traversal, TraversalScope, VectorIndex,
};
use mneme_core::{
    BodyRef, BodySpan, Edge, EdgeKind, EmbeddingFingerprint, MAX_DERIVED_SOURCES,
    MAX_NODE_SUMMARY_BYTES, MAX_REMOTE_EDGE_PAGE_SIZE, MergeResolution, Node, NodeId, NodeStatus,
    Provenance, RemoteEdge, Resolution, StrengthParams,
};
use mneme_cozo::{CozoStore, MAX_CANONICAL_NODE_JSON_BYTES, MemStore, StoreExportEnvelopeV5};
use ulid::Ulid;
use unordered_pair::UnorderedPair;

const _: fn(
    &std::path::Path,
    std::sync::Arc<mneme_store_path::StoreLease>,
) -> std::result::Result<CozoStore, Error> = CozoStore::open_leased_current;
const _: fn(
    &std::path::Path,
    usize,
    std::sync::Arc<mneme_store_path::StoreLease>,
) -> std::result::Result<CozoStore, Error> = CozoStore::open_persistent;
const _: fn(
    &std::path::Path,
    usize,
    std::sync::Arc<mneme_store_path::StoreLease>,
) -> std::result::Result<CozoStore, Error> = CozoStore::open_existing_persistent;

const DIM: usize = 8;
const VECTOR_META_KEY: &str = "vector_projection_v2";
const PRIOR_V3_VECTOR_META: &str = "status_partitioned_hnsw_mnestic_0_13_v3";
const CANONICAL_NODE_KEY: &str = "canonical_node_contract_v2";
const CURRENT_TAG_PROJECTION_META: &str = "canonical_node_tag_membership_v2_f7";

fn remove_sqlite_files(path: &std::path::Path) {
    let base = path.as_os_str().to_string_lossy();
    for suffix in ["", "-wal", "-shm", "-journal"] {
        let _ = std::fs::remove_file(format!("{base}{suffix}"));
    }
}

fn reopen_current(path: &std::path::Path) -> CozoStore {
    // macOS temp paths can spell /var through /private/var. Snapshot admission
    // binds the canonical pathname to the opened file, so use one spelling.
    let canonical = path.canonicalize().unwrap();
    let lease = std::sync::Arc::new(mneme_store_path::StoreLease::acquire(&canonical).unwrap());
    CozoStore::open_leased_current(&canonical, lease).unwrap()
}

fn ordered_meta(raw: &cozo::DbInstance) -> Vec<Vec<cozo::DataValue>> {
    raw.run_default("?[k, v] := *meta{k, v} :order k")
        .unwrap()
        .rows
}

fn node(summary: &str, tags: &[&str]) -> Node {
    node_with_id(NodeId(Ulid::new()), summary, tags)
}

fn node_with_id(id: NodeId, summary: &str, tags: &[&str]) -> Node {
    Node::try_new(
        id,
        summary,
        BodyRef::new("inline://x").unwrap(),
        tags.iter().copied(),
        Provenance::derived_empty(),
        0.5,
        0.5,
        NodeStatus::Active,
        1_000,
    )
    .unwrap()
}

async fn active_tagged_batch(store: &CozoStore, query: &[f32], tags: &[&str]) -> TaggedAnnBatch {
    let lanes = [TaggedAnnLaneRequest::new(
        mneme_core::tagged::RetrievalLifecycleLane::Primary,
        StatusFilter::ACTIVE,
        4,
        8,
    )
    .unwrap()];
    store
        .tagged_ann(
            TaggedAnnRequest::new(
                query,
                tags.iter().copied(),
                &lanes,
                TaggedAnnWorkLimits::default(),
            )
            .unwrap(),
        )
        .await
        .unwrap()
}

fn assert_exact_single_tag(batch: &TaggedAnnBatch, expected: Option<NodeId>) {
    assert_eq!(
        batch.projection_generation.as_str(),
        CURRENT_TAG_PROJECTION_META
    );
    assert_eq!(
        batch.lanes[0].seed_coverage,
        TaggedSeedCoverage::ExactCosine
    );
    assert_eq!(batch.work.raw_memberships, usize::from(expected.is_some()));
    assert_eq!(batch.work.unique_exact_ids, usize::from(expected.is_some()));
    assert_eq!(
        batch.work.exact_hydrated_ids,
        usize::from(expected.is_some())
    );
    assert_eq!(
        batch.work.exact_vector_components,
        usize::from(expected.is_some()) * DIM
    );
    assert_eq!(batch.lanes[0].hits.first().map(|hit| hit.id), expected);
    assert_eq!(batch.work.fallback_hnsw_inspected, 0);
    assert_eq!(batch.work.fallback_sample_inspected, 0);
}

/// A trivial deterministic embedding: bucket token-length sums. Enough to make
/// HNSW return the right neighbor for an identical vector.
fn embed(seed: f32) -> Vec<f32> {
    let mut v = vec![0.0f32; DIM];
    v[0] = seed;
    v[1] = seed * 0.5;
    v
}

fn fingerprint(id: &str, dim: usize) -> EmbeddingFingerprint {
    EmbeddingFingerprint::new(id, dim, "l2-f32-v1", "symmetric-v1")
}

async fn assert_vector_projection_parity(store: &(impl GraphStore + VectorIndex)) {
    let query = embed(1.0);
    let orphan = NodeId(Ulid::new());
    assert!(matches!(
        store.upsert(orphan, &query).await,
        Err(Error::NotFound)
    ));

    let mut projected = node("projection parity", &[]);
    store.put_node(&projected).await.unwrap();
    for invalid in [vec![0.0; DIM], vec![f32::MAX; DIM], {
        let mut vector = query.clone();
        vector[0] = f32::NAN;
        vector
    }] {
        assert!(matches!(
            store.upsert(projected.id(), &invalid).await,
            Err(Error::InvalidInput(_))
        ));
        assert!(matches!(
            store.ann(&invalid, 1, StatusFilter::ACTIVE).await,
            Err(Error::InvalidInput(_))
        ));
    }
    assert!(matches!(
        store.upsert(projected.id(), &query[..DIM - 1]).await,
        Err(Error::DimMismatch {
            index: DIM,
            provider
        }) if provider == DIM - 1
    ));
    assert!(matches!(
        store
            .ann(&query[..DIM - 1], 1, StatusFilter::ACTIVE)
            .await,
        Err(Error::DimMismatch {
            index: DIM,
            provider
        }) if provider == DIM - 1
    ));
    store.upsert(projected.id(), &query).await.unwrap();
    assert_eq!(
        store
            .ann(&query, 1, StatusFilter::ACTIVE)
            .await
            .unwrap()
            .first()
            .map(|hit| hit.id),
        Some(projected.id())
    );

    projected.set_status(NodeStatus::Archived);
    store.put_node(&projected).await.unwrap();
    assert!(
        store
            .ann(&query, 1, StatusFilter::ACTIVE)
            .await
            .unwrap()
            .is_empty()
    );
    assert_eq!(
        store
            .ann(&query, 1, StatusFilter::ARCHIVED)
            .await
            .unwrap()
            .first()
            .map(|hit| hit.id),
        Some(projected.id())
    );

    store.delete_node(projected.id()).await.unwrap();
    assert!(
        store
            .ann(&query, 1, StatusFilter::ALL)
            .await
            .unwrap()
            .is_empty(),
        "deleting a node must atomically retire its vector projection"
    );
}

#[tokio::test]
async fn embedding_identity_initializes_only_empty_stores() {
    let store = CozoStore::new(DIM).unwrap();
    let current = fingerprint("test:embedder-v1", DIM);
    assert_eq!(
        store.ensure_embedding_fingerprint(&current).unwrap(),
        EmbeddingFingerprintInit::InitializedEmptyStore
    );
    assert_eq!(
        store.embedding_fingerprint().unwrap(),
        Some(current.clone())
    );

    let mut other = current;
    other.query_mode = "asymmetric-query-prefix-v2".into();
    assert!(matches!(
        store.ensure_embedding_fingerprint(&other),
        Err(Error::EmbeddingFingerprintMismatch { .. })
    ));
}

#[tokio::test]
async fn populated_legacy_cozo_store_fails_closed() {
    let store = CozoStore::new(DIM).unwrap();
    let n = node("legacy vector", &[]);
    store.put_node(&n).await.unwrap();
    store.upsert(n.id(), &embed(1.0)).await.unwrap();

    let current = fingerprint("test:embedder-v1", DIM);
    assert!(matches!(
        store.ensure_embedding_fingerprint(&current),
        Err(Error::LegacyEmbeddingFingerprint)
    ));
    assert_eq!(store.embedding_fingerprint().unwrap(), None);
}

#[tokio::test]
async fn embedding_identity_survives_persistent_reopen() {
    let path = std::env::temp_dir().join(format!("mneme-cozo-fingerprint-{}.db", Ulid::new()));
    let p = path.to_str().unwrap();
    let current = fingerprint("test:embedder-v1", DIM);
    {
        let store = CozoStore::open(p, DIM).unwrap();
        store.ensure_embedding_fingerprint(&current).unwrap();
        store.prepare_for_file_move().unwrap();
    }
    let reopened = reopen_current(&path);
    assert_eq!(reopened.embedding_fingerprint().unwrap(), Some(current));
    drop(reopened);
    let _ = std::fs::remove_file(&path);
}

#[tokio::test]
async fn node_round_trips_through_cozo() {
    let store = CozoStore::new(DIM).unwrap();
    let n = node("a memory node about graphs", &["graph", "memory"]);
    let id = n.id();
    store.put_node(&n).await.unwrap();

    let got = store.get_node(id).await.unwrap().expect("node present");
    assert_eq!(got.summary(), "a memory node about graphs");
    assert!(got.has_tag("graph") && got.has_tag("memory"));
    assert!(got.is_active());

    store.set_status(id, NodeStatus::Archived).await.unwrap();
    assert!(store.get_node(id).await.unwrap().unwrap().is_archived());

    assert_eq!(store.all_nodes(ColdPath::acquire()).await.unwrap().len(), 1);
}

#[tokio::test]
async fn persistent_tagged_replacement_is_exact_across_every_reopen() {
    let path = std::env::temp_dir().join(format!("mneme-cozo-tag-replace-{}.db", Ulid::new()));
    let p = path.to_str().unwrap();
    let id = NodeId(Ulid::new());
    let query = embed(1.0);

    {
        let store = CozoStore::open(p, DIM).unwrap();
        store
            .put_node(&node_with_id(id, "tag replacement", &["a", "b"]))
            .await
            .unwrap();
        store.upsert(id, &query).await.unwrap();
        store.prepare_for_file_move().unwrap();
    }
    {
        let store = reopen_current(&path);
        assert_exact_single_tag(&active_tagged_batch(&store, &query, &["a"]).await, Some(id));
        assert_exact_single_tag(&active_tagged_batch(&store, &query, &["b"]).await, Some(id));
        store
            .put_node(&node_with_id(id, "tag replacement", &["b", "c"]))
            .await
            .unwrap();
        store.prepare_for_file_move().unwrap();
    }
    {
        let store = reopen_current(&path);
        assert_exact_single_tag(&active_tagged_batch(&store, &query, &["a"]).await, None);
        assert_exact_single_tag(&active_tagged_batch(&store, &query, &["b"]).await, Some(id));
        assert_exact_single_tag(&active_tagged_batch(&store, &query, &["c"]).await, Some(id));
        store
            .put_node(&node_with_id(id, "tag replacement", &[]))
            .await
            .unwrap();
        store.prepare_for_file_move().unwrap();
    }
    {
        let store = reopen_current(&path);
        for tag in ["a", "b", "c"] {
            assert_exact_single_tag(&active_tagged_batch(&store, &query, &[tag]).await, None);
        }
        assert_eq!(
            store
                .ann(&query, 1, StatusFilter::ACTIVE)
                .await
                .unwrap()
                .first()
                .map(|hit| hit.id),
            Some(id),
            "empty-tag replacement must preserve the canonical vector"
        );
    }
    remove_sqlite_files(&path);
}

#[tokio::test]
async fn vector_projection_invariants_match_reference_backend() {
    let mem = MemStore::new(DIM);
    assert_vector_projection_parity(&mem).await;

    let cozo = CozoStore::new(DIM).unwrap();
    assert_vector_projection_parity(&cozo).await;
}

#[tokio::test]
async fn lexical_index_is_lifecycle_exact_and_removes_stale_postings() {
    let path = std::env::temp_dir().join(format!("mneme-cozo-fts-{}.db", Ulid::new()));
    let p = path.to_str().unwrap();
    let id;
    {
        let store = CozoStore::open(p, DIM).unwrap();
        let original = node("deployment marker zxq771", &[]);
        id = original.id();
        store.put_node(&original).await.unwrap();
        assert_eq!(
            store
                .search("ZXQ771", 4, StatusFilter::ACTIVE)
                .await
                .unwrap()
                .iter()
                .map(|hit| hit.id)
                .collect::<Vec<_>>(),
            vec![id]
        );

        store.set_status(id, NodeStatus::Archived).await.unwrap();
        assert!(
            store
                .search("zxq771", 4, StatusFilter::ACTIVE)
                .await
                .unwrap()
                .is_empty(),
            "an archived row must leave the active FTS index"
        );
        assert_eq!(
            store
                .search("zxq771", 4, StatusFilter::ARCHIVED)
                .await
                .unwrap()[0]
                .id,
            id
        );

        let replacement = Node::try_new(
            id,
            "replacement marker qqq999",
            BodyRef::new("inline://x").unwrap(),
            std::iter::empty::<&str>(),
            Provenance::derived_empty(),
            0.5,
            0.5,
            NodeStatus::Active,
            1_000,
        )
        .unwrap();
        store.put_node(&replacement).await.unwrap();
        assert!(
            store
                .search("zxq771", 4, StatusFilter::ALL)
                .await
                .unwrap()
                .is_empty(),
            "summary updates must delete old token postings"
        );
        assert_eq!(
            store
                .search("qqq999", 4, StatusFilter::ACTIVE)
                .await
                .unwrap()[0]
                .id,
            id
        );
        store.prepare_for_file_move().unwrap();
    }

    let reopened = reopen_current(&path);
    assert_eq!(
        reopened
            .search("qqq999", 4, StatusFilter::ACTIVE)
            .await
            .unwrap()[0]
            .id,
        id,
        "the derived FTS index must survive a persistent reopen"
    );
    reopened.delete_node(id).await.unwrap();
    assert!(
        reopened
            .search("qqq999", 4, StatusFilter::ALL)
            .await
            .unwrap()
            .is_empty(),
        "deletion must remove native FTS postings"
    );
    drop(reopened);
    let _ = std::fs::remove_file(path);
}

#[tokio::test]
async fn missing_current_lexical_projection_refuses_reopen_without_repair() {
    let path = std::env::temp_dir().join(format!("mneme-cozo-missing-fts-{}.db", Ulid::new()));
    let p = path.to_str().unwrap();
    let id = {
        let store = CozoStore::open(p, DIM).unwrap();
        let memory = node("retained search marker RETRO771", &[]);
        store.put_node(&memory).await.unwrap();
        store.prepare_for_file_move().unwrap();
        memory.id()
    };

    // Removing a successor derived index is a torn current catalog, not an
    // invitation for ordinary open to silently backfill it.
    let before = {
        let raw = cozo::DbInstance::new("sqlite", p, "").unwrap();
        raw.run_default("::fts drop node_search:active_fts")
            .unwrap();
        raw.run_default("::fts drop node_search:archived_fts")
            .unwrap();
        raw.run_default("::index drop node_search:by_status")
            .unwrap();
        raw.run_default("::remove node_search").unwrap();
        raw.run_default("?[k] := *meta{k}, k == 'lexical_projection_v1' :rm meta {k}")
            .unwrap();
        (
            ordered_meta(&raw),
            raw.run_default(&format!(
                "?[data, status] := *node{{id: '{}', data, status}}",
                id.0
            ))
            .unwrap()
            .rows,
        )
    };
    let lease = mneme_store_path::StoreLease::acquire(&path).unwrap();
    assert!(CozoStore::require_existing_current(&path, &lease).is_err());
    drop(lease);
    let raw = cozo::DbInstance::new("sqlite", p, "").unwrap();
    assert_eq!(ordered_meta(&raw), before.0);
    assert_eq!(
        raw.run_default(&format!(
            "?[data, status] := *node{{id: '{}', data, status}}",
            id.0
        ))
        .unwrap()
        .rows,
        before.1
    );
    assert!(raw.run_default("::columns node_search").is_err());
    drop(raw);
    remove_sqlite_files(&path);
}

#[tokio::test]
async fn cozo_db_id_is_stable_across_reopen() {
    let path = std::env::temp_dir().join(format!("mneme-cozo-{}.db", Ulid::new()));
    let p = path.to_str().unwrap();
    let id = {
        let store = CozoStore::open(p, DIM).unwrap();
        let id = store.db_id();
        store.prepare_for_file_move().unwrap();
        id
    };
    let again = reopen_current(&path).db_id();
    let _ = std::fs::remove_file(&path);
    assert_eq!(id, again, "the cozo db id persists across reopen");
}

#[tokio::test]
async fn hnsw_ann_finds_nearest_and_skips_archived() {
    let store = CozoStore::new(DIM).unwrap();
    let a = node("alpha", &["x"]);
    let b = node("beta", &["y"]);
    store.put_node(&a).await.unwrap();
    store.put_node(&b).await.unwrap();
    store.upsert(a.id(), &embed(1.0)).await.unwrap();
    store.upsert(b.id(), &embed(-1.0)).await.unwrap();

    let active = StatusFilter::ACTIVE;
    let hits = store.ann(&embed(1.0), 2, active).await.unwrap();
    assert_eq!(
        hits.first().map(|s| s.id),
        Some(a.id()),
        "nearest should be a"
    );

    // Archived nodes drop out of the default search, but `ALL` digs them up.
    store
        .set_status(a.id(), NodeStatus::Archived)
        .await
        .unwrap();
    let hits = store.ann(&embed(1.0), 2, active).await.unwrap();
    assert!(
        hits.iter().all(|s| s.id != a.id()),
        "archived node must not surface by default"
    );
    let dug = store.ann(&embed(1.0), 2, StatusFilter::ALL).await.unwrap();
    assert!(
        dug.iter().any(|s| s.id == a.id()),
        "ALL surfaces the archived node"
    );
}

#[tokio::test]
async fn archived_hnsw_is_exact_beyond_the_active_window() {
    let store = CozoStore::new(DIM).unwrap();
    let query = embed(1.0);

    // More active exact matches than any reasonable fixed over-fetch window.
    // Archived-only lookup must still find the archived vector rather than
    // filtering an all-status HNSW window after the fact.
    for i in 0..40 {
        let active = node(&format!("active distractor {i}"), &[]);
        store.put_node(&active).await.unwrap();
        store.upsert(active.id(), &query).await.unwrap();
    }

    let mut archived = node("strong archived neighbor", &[]);
    archived.set_status(NodeStatus::Archived);
    let mut archived_vector = vec![0.0; DIM];
    archived_vector[0] = 1.0;
    archived_vector[1] = 0.8;
    store.put_node(&archived).await.unwrap();
    store.upsert(archived.id(), &archived_vector).await.unwrap();

    let hits = store.ann(&query, 1, StatusFilter::ARCHIVED).await.unwrap();
    assert_eq!(hits.first().map(|hit| hit.id), Some(archived.id()));
    assert!(
        store
            .ann(&query, 1, StatusFilter::ACTIVE)
            .await
            .unwrap()
            .iter()
            .all(|hit| hit.id != archived.id())
    );

    // Lifecycle transitions update the side index, and returning to Archived
    // restores the already-stored vector without requiring re-embedding.
    store
        .set_status(archived.id(), NodeStatus::Active)
        .await
        .unwrap();
    assert!(
        store
            .ann(&query, 1, StatusFilter::ARCHIVED)
            .await
            .unwrap()
            .is_empty()
    );
    store
        .set_status(archived.id(), NodeStatus::Archived)
        .await
        .unwrap();
    assert_eq!(
        store
            .ann(&query, 1, StatusFilter::ARCHIVED)
            .await
            .unwrap()
            .first()
            .map(|hit| hit.id),
        Some(archived.id())
    );
}

#[tokio::test]
async fn missing_canonical_node_contract_fails_before_open_mutations() {
    let path = std::env::temp_dir().join(format!("mneme-cozo-pre-node-{}.db", Ulid::new()));
    let p = path.to_str().unwrap();
    let store = CozoStore::open(p, DIM).unwrap();
    store.prepare_for_file_move().unwrap();
    drop(store);

    let before = {
        let raw = cozo::DbInstance::new("sqlite", p, "").unwrap();
        raw.run_default(&format!(
            "?[k] := *meta{{k}}, k == '{CANONICAL_NODE_KEY}' :rm meta {{k}}"
        ))
        .unwrap();
        raw.run_default("::remove supersede_commit").unwrap();
        raw.run_default(
            "?[key, fingerprint, applied_at] <- [['retained', 'proof', 7]] \
             :put feedback_retry {key => fingerprint, applied_at}",
        )
        .unwrap();
        raw.run_default("?[k] := *meta{k}, k == 'max_incident_edges_v1' :rm meta {k}")
            .unwrap();
        ordered_meta(&raw)
    };

    let lease = mneme_store_path::StoreLease::acquire(&path).unwrap();
    assert!(
        CozoStore::require_existing_current(&path, &lease).is_err(),
        "leased current admission must reject a torn canonical contract"
    );
    drop(lease);

    let raw = cozo::DbInstance::new("sqlite", p, "").unwrap();
    assert_eq!(ordered_meta(&raw), before, "rejected open rewrote metadata");
    assert!(
        raw.run_default("::columns supersede_commit").is_err(),
        "rejected open performed additive relation repair"
    );
    assert_eq!(
        raw.run_default("?[key] := *feedback_retry{key}")
            .unwrap()
            .rows
            .len(),
        1,
        "rejected open cleared the retry ledger"
    );
    drop(raw);
    remove_sqlite_files(&path);
}

#[test]
fn marker_and_vector_sentinel_disagreement_is_corruption() {
    enum Disagreement {
        CurrentWithoutCompanion,
        PriorWithFutureMetadata,
        VectorAbsent,
    }

    for (index, disagreement) in [
        Disagreement::CurrentWithoutCompanion,
        Disagreement::PriorWithFutureMetadata,
        Disagreement::VectorAbsent,
    ]
    .into_iter()
    .enumerate()
    {
        let path = std::env::temp_dir().join(format!(
            "mneme-cozo-contract-disagreement-{index}-{}.db",
            Ulid::new()
        ));
        let p = path.to_str().unwrap();
        let store = CozoStore::open(p, DIM).unwrap();
        store.prepare_for_file_move().unwrap();
        drop(store);
        let before = {
            let raw = cozo::DbInstance::new("sqlite", p, "").unwrap();
            match disagreement {
                Disagreement::CurrentWithoutCompanion => {
                    raw.run_default(&format!(
                        "?[k] := *meta{{k}}, k == '{CANONICAL_NODE_KEY}' :rm meta {{k}}"
                    ))
                    .unwrap();
                }
                Disagreement::PriorWithFutureMetadata => {
                    raw.run_default(&format!(
                        "?[k, v] <- [['{VECTOR_META_KEY}', '{PRIOR_V3_VECTOR_META}']] \
                         :put meta {{k => v}}"
                    ))
                    .unwrap();
                }
                Disagreement::VectorAbsent => {
                    raw.run_default(&format!(
                        "?[k] := *meta{{k}}, k == '{VECTOR_META_KEY}' :rm meta {{k}}"
                    ))
                    .unwrap();
                }
            }
            ordered_meta(&raw)
        };

        let lease = mneme_store_path::StoreLease::acquire(&path).unwrap();
        assert!(
            CozoStore::require_existing_current(&path, &lease).is_err(),
            "marker/sentinel disagreement must fail closed"
        );
        drop(lease);
        let raw = cozo::DbInstance::new("sqlite", p, "").unwrap();
        assert_eq!(ordered_meta(&raw), before);
        drop(raw);
        remove_sqlite_files(&path);
    }
}

#[tokio::test]
async fn malformed_current_canonical_rows_refuse_read_without_repairing_source() {
    #[derive(Clone, Copy)]
    enum Corruption {
        BodyRef,
        WebUrl,
        OriginCommit,
        BlankSummary,
        LongSummary,
        DuplicateDerived,
        TooManyDerived,
        NanScalar,
        OutOfRangeScalar,
        OversizedUnknown,
        BlobId,
        ProjectedStatus,
    }

    for (label, corruption, _invariant) in [
        ("body", Corruption::BodyRef, "body reference"),
        ("web", Corruption::WebUrl, "web URL"),
        ("origin", Corruption::OriginCommit, "origin commit"),
        (
            "blank-summary",
            Corruption::BlankSummary,
            "node summary must not be blank",
        ),
        ("long-summary", Corruption::LongSummary, "node summary is"),
        (
            "duplicate-derived",
            Corruption::DuplicateDerived,
            "derived sources contains duplicate",
        ),
        (
            "too-many-derived",
            Corruption::TooManyDerived,
            "derived sources has more than",
        ),
        ("nan-scalar", Corruption::NanScalar, "expected value"),
        (
            "out-of-range-scalar",
            Corruption::OutOfRangeScalar,
            "stability must be finite and between 0 and 1",
        ),
        (
            "oversized-unknown",
            Corruption::OversizedUnknown,
            "serialized blob is",
        ),
        ("blob-id", Corruption::BlobId, "does not match row key"),
        (
            "projected-status",
            Corruption::ProjectedStatus,
            "does not match projected status",
        ),
    ] {
        let path = std::env::temp_dir().join(format!(
            "mneme-cozo-invalid-node-{label}-{}.db",
            Ulid::new()
        ));
        let p = path.to_str().unwrap();
        let retained = node("invalid canonical Node fixture", &[]);
        {
            let store = CozoStore::open(p, DIM).unwrap();
            store.put_node(&retained).await.unwrap();
            store.prepare_for_file_move().unwrap();
        }

        let (before_node, before_meta) = {
            let raw = cozo::DbInstance::new("sqlite", p, "").unwrap();
            let data = raw
                .run_default(&format!(
                    "?[data] := *node{{id: '{}', data}}",
                    retained.id().0
                ))
                .unwrap();
            let mut value: serde_json::Value =
                serde_json::from_str(data.rows[0][0].get_str().unwrap()).unwrap();
            value["provenance"] = serde_json::json!({
                "Web": {"url": "https://example.test/source", "fetched": 7}
            });
            value["origin_commit"] = serde_json::json!("0123456789abcdef0123456789abcdef01234567");
            match corruption {
                Corruption::BodyRef => {
                    value["body"] = serde_json::json!(format!(
                        "x://{}",
                        "a".repeat(mneme_core::MAX_BODY_REF_BYTES)
                    ));
                }
                Corruption::WebUrl => {
                    value["provenance"]["Web"]["url"] =
                        serde_json::json!("ftp://example.test/source");
                }
                Corruption::OriginCommit => {
                    value["origin_commit"] = serde_json::json!("deadbeef");
                }
                Corruption::BlankSummary => {
                    value["summary"] = serde_json::json!(" \n\t");
                }
                Corruption::LongSummary => {
                    value["summary"] = serde_json::json!("s".repeat(MAX_NODE_SUMMARY_BYTES + 1));
                }
                Corruption::DuplicateDerived => {
                    let source = NodeId(Ulid::from(41u128));
                    value["provenance"] = serde_json::json!({
                        "Derived": {"from": [source, source]}
                    });
                }
                Corruption::TooManyDerived => {
                    let sources = (0..=MAX_DERIVED_SOURCES)
                        .map(|index| NodeId(Ulid::from(100u128 + index as u128)))
                        .collect::<Vec<_>>();
                    value["provenance"] = serde_json::json!({"Derived": {"from": sources}});
                }
                Corruption::NanScalar => {}
                Corruption::OutOfRangeScalar => {
                    value["stability"] = serde_json::json!(1.1);
                }
                Corruption::OversizedUnknown => {
                    value["ignored_but_bounded"] =
                        serde_json::json!("x".repeat(MAX_CANONICAL_NODE_JSON_BYTES));
                }
                Corruption::BlobId => {
                    value["id"] = serde_json::json!(Ulid::new().to_string());
                }
                Corruption::ProjectedStatus => {}
            }
            let mut encoded = serde_json::to_string(&value).unwrap();
            if matches!(corruption, Corruption::NanScalar) {
                let prior = encoded.clone();
                encoded = encoded.replacen("\"confidence\":0.5", "\"confidence\":NaN", 1);
                assert_ne!(encoded, prior, "fixture did not locate confidence scalar");
            }
            if matches!(corruption, Corruption::OversizedUnknown) {
                assert!(encoded.len() > MAX_CANONICAL_NODE_JSON_BYTES);
            }
            let projected_status = if matches!(corruption, Corruption::ProjectedStatus) {
                "archived"
            } else {
                "active"
            };
            let mut params = std::collections::BTreeMap::new();
            params.insert(
                "id".into(),
                cozo::DataValue::from(retained.id().0.to_string()),
            );
            params.insert("data".into(), cozo::DataValue::from(encoded));
            params.insert("status".into(), cozo::DataValue::from(projected_status));
            raw.run_script(
                "?[id, data, status] <- [[$id, $data, $status]] \
                 :put node {id => data, status}",
                params,
                cozo::ScriptMutability::Mutable,
            )
            .unwrap();
            let source = raw
                .run_default(&format!(
                    "?[data, status] := *node{{id: '{}', data, status}}",
                    retained.id().0
                ))
                .unwrap()
                .rows;
            (source, ordered_meta(&raw))
        };

        let lease = std::sync::Arc::new(mneme_store_path::StoreLease::acquire(&path).unwrap());
        match CozoStore::open_leased_current(&path, lease) {
            Ok(current) => {
                assert!(
                    current.get_node(retained.id()).await.is_err(),
                    "{label} malformed canonical row was returned as a valid node"
                );
                drop(current);
            }
            Err(_closed_catalog_refusal) => {
                // A raw current-generation classifier may reject the malformed
                // row before the runtime ever receives a handle.
            }
        }

        let raw = cozo::DbInstance::new("sqlite", p, "").unwrap();
        assert_eq!(
            raw.run_default(&format!(
                "?[data, status] := *node{{id: '{}', data, status}}",
                retained.id().0
            ))
            .unwrap()
            .rows,
            before_node,
            "{label} rejected read rewrote its canonical source row"
        );
        assert_eq!(
            ordered_meta(&raw),
            before_meta,
            "{label} rejected read rewrote canonical metadata"
        );
        drop(raw);
        remove_sqlite_files(&path);
    }
}

#[tokio::test]
async fn current_torn_dimension_and_identity_refuse_without_repair() {
    let path = std::env::temp_dir().join(format!("mneme-cozo-vector-v2-dim-{}.db", Ulid::new()));
    let p = path.to_str().unwrap();
    let retained = {
        let store = CozoStore::open(p, DIM).unwrap();
        let retained = node("retained current vector", &[]);
        store.put_node(&retained).await.unwrap();
        store.upsert(retained.id(), &embed(1.0)).await.unwrap();
        store.prepare_for_file_move().unwrap();
        retained.id()
    };
    {
        let raw = cozo::DbInstance::new("sqlite", p, "").unwrap();
        raw.run_default("?[k] := *meta{k}, k == 'db_id' :rm meta {k}")
            .unwrap();
        raw.run_default("?[k, v] <- [['dim', '768']] :put meta {k => v}")
            .unwrap();
    }

    let lease = mneme_store_path::StoreLease::acquire(&path).unwrap();
    assert!(
        CozoStore::require_existing_current(&path, &lease).is_err(),
        "a torn current dimension/identity must not be admitted"
    );

    let raw = cozo::DbInstance::new("sqlite", p, "").unwrap();
    assert!(
        raw.run_default("?[v] := *meta{k: 'db_id', v}")
            .unwrap()
            .rows
            .is_empty(),
        "rejected current admission must not repair db_id"
    );
    assert_eq!(
        raw.run_default("?[v] := *meta{k: 'dim', v}").unwrap().rows[0][0].get_str(),
        Some("768")
    );
    assert!(
        raw.run_default("?[id] := *node_vec{id}")
            .unwrap()
            .rows
            .iter()
            .any(|row| row[0].get_str() == Some(retained.0.to_string().as_str())),
        "rejected current admission must leave the canonical vector relation untouched"
    );
    drop(raw);
    drop(lease);
    let _ = std::fs::remove_file(&path);
}

#[tokio::test]
async fn edges_reinforce_and_neighbors_query() {
    let store = CozoStore::new(DIM).unwrap();
    let a = node("a", &[]);
    let b = node("b", &[]);
    store.put_node(&a).await.unwrap();
    store.put_node(&b).await.unwrap();

    let edge = mneme_core::Edge::new(a.id(), b.id(), 0.4, EdgeKind::Associative, 1_000);
    store.put_edge(&edge).await.unwrap();

    let nbrs = store.neighbors(a.id(), 8).await.unwrap();
    assert_eq!(nbrs.len(), 1);
    assert_eq!(nbrs[0].node, b.id());
    assert!(!nbrs[0].incoming, "a->b surfaces as outgoing on a");
    // From b, the same associative edge is navigable in reverse (incoming).
    let from_b = store.neighbors(b.id(), 8).await.unwrap();
    assert_eq!(from_b.len(), 1);
    assert_eq!(from_b[0].node, a.id());
    assert!(
        from_b[0].incoming,
        "b sees a as an incoming (lazy reverse) edge"
    );

    // Reinforce via read-modify-write (the store no longer owns the formula).
    // Diminishing returns from a 0.4 prior: 0.4 + (1-0.4)*0.35 = 0.61.
    let mut edge = store.get_edge(a.id(), b.id()).await.unwrap().unwrap();
    edge.reinforce(2_000, &StrengthParams::default());
    store.put_edge(&edge).await.unwrap();
    let reinforced = store.get_edge(a.id(), b.id()).await.unwrap().unwrap();
    assert!((reinforced.weight() - 0.61).abs() < 1e-5);
    assert_eq!(reinforced.trials(), 1);
    assert_eq!(reinforced.last_reinforced(), 2_000);
}

#[tokio::test]
async fn learned_transition_only_surfaces_in_the_observed_direction() {
    let store = CozoStore::new(DIM).unwrap();
    let prior = node("prior", &[]);
    let target = node("target", &[]);
    store.put_node(&prior).await.unwrap();
    store.put_node(&target).await.unwrap();
    store
        .put_edge(&Edge::new(
            prior.id(),
            target.id(),
            0.8,
            EdgeKind::Transition,
            1,
        ))
        .await
        .unwrap();

    let forward = store.neighbors(prior.id(), 8).await.unwrap();
    assert_eq!(forward.len(), 1);
    assert_eq!(forward[0].node, target.id());
    assert!(!forward[0].incoming);
    assert!(store.neighbors(target.id(), 8).await.unwrap().is_empty());
}

#[tokio::test]
async fn edge_anchor_replace_clear_and_delete_stay_in_sync() {
    let store = CozoStore::new(DIM).unwrap();
    let a = node("anchor source", &[]);
    let b = node("anchor target", &[]);
    store.put_node(&a).await.unwrap();
    store.put_node(&b).await.unwrap();

    let mut edge = Edge::new(a.id(), b.id(), 0.4, EdgeKind::Associative, 1);
    edge.anchor = Some(BodySpan::new(2, 8));
    store.put_edge(&edge).await.unwrap();
    let anchor = store
        .get_edge(a.id(), b.id())
        .await
        .unwrap()
        .unwrap()
        .anchor
        .unwrap();
    assert_eq!((anchor.start, anchor.end), (2, 8));

    edge.anchor = Some(BodySpan::new(5, 13));
    store.put_edge(&edge).await.unwrap();
    let anchor = store
        .get_edge(a.id(), b.id())
        .await
        .unwrap()
        .unwrap()
        .anchor
        .unwrap();
    assert_eq!((anchor.start, anchor.end), (5, 13));

    edge.anchor = None;
    store.put_edge(&edge).await.unwrap();
    assert!(
        store
            .get_edge(a.id(), b.id())
            .await
            .unwrap()
            .unwrap()
            .anchor
            .is_none()
    );

    edge.anchor = Some(BodySpan::new(1, 3));
    store.put_edge(&edge).await.unwrap();
    store.delete_edge(a.id(), b.id()).await.unwrap();
    assert!(store.get_edge(a.id(), b.id()).await.unwrap().is_none());
}

#[tokio::test]
async fn spread_and_communities_over_cozo() {
    let store = CozoStore::new(DIM).unwrap();
    let mut ids = Vec::new();
    for s in ["g1", "g2", "h1", "h2"] {
        let n = node(s, &[]);
        ids.push(n.id());
        store.put_node(&n).await.unwrap();
    }
    let link = |from: NodeId, to: NodeId, w: f32| {
        mneme_core::Edge::new(from, to, w, EdgeKind::Associative, 1)
    };
    // Two disjoint pairs.
    store.put_edge(&link(ids[0], ids[1], 0.9)).await.unwrap();
    store.put_edge(&link(ids[2], ids[3], 0.9)).await.unwrap();

    // Spread from g1 reaches g2 but not the other component.
    let spread = store
        .spread(
            &[Scored {
                id: ids[0],
                score: 1.0,
            }],
            Budget::default(),
            None,
            TraversalScope::new(StatusFilter::ACTIVE),
        )
        .await
        .unwrap();
    let reached: Vec<NodeId> = spread.iter().map(|s| s.id).collect();
    assert!(reached.contains(&ids[1]));
    assert!(!reached.contains(&ids[2]));

    let labels: std::collections::HashMap<_, _> = store
        .detect_communities(ColdPath::acquire())
        .await
        .unwrap()
        .into_iter()
        .collect();
    assert_eq!(labels[&ids[0]], labels[&ids[1]]);
    assert_eq!(labels[&ids[2]], labels[&ids[3]]);
    assert_ne!(labels[&ids[0]], labels[&ids[2]]);
}

#[tokio::test]
async fn cozo_active_spread_does_not_surface_or_route_through_hidden_statuses() {
    let store = CozoStore::new(DIM).unwrap();
    let seed = node("seed", &[]);
    let archived = node("archived bridge", &[]);
    let active_bridge = node("ordinary active bridge", &[]);
    let via_archived = node("active behind archived", &[]);
    let via_active = node("active behind ordinary bridge", &[]);
    let direct = node("direct active", &[]);
    for item in [
        &seed,
        &archived,
        &active_bridge,
        &via_archived,
        &via_active,
        &direct,
    ] {
        store.put_node(item).await.unwrap();
    }
    store
        .set_status(archived.id(), NodeStatus::Archived)
        .await
        .unwrap();
    for (from, to, weight) in [
        (seed.id(), archived.id(), 1.0),
        (archived.id(), via_archived.id(), 1.0),
        (seed.id(), active_bridge.id(), 0.99),
        (active_bridge.id(), via_active.id(), 1.0),
        (seed.id(), direct.id(), 0.5),
    ] {
        store
            .put_edge(&mneme_core::Edge::new(
                from,
                to,
                weight,
                EdgeKind::Associative,
                1,
            ))
            .await
            .unwrap();
    }

    let budget = Budget {
        max_depth: 3,
        explore: 0.0,
        ..Budget::default()
    };
    let active = store
        .spread(
            &[Scored {
                id: seed.id(),
                score: 1.0,
            }],
            budget,
            None,
            TraversalScope::new(StatusFilter::ACTIVE),
        )
        .await
        .unwrap();
    let ids: std::collections::HashSet<NodeId> = active.iter().map(|item| item.id).collect();
    assert_eq!(
        ids,
        std::collections::HashSet::from([
            seed.id(),
            active_bridge.id(),
            via_active.id(),
            direct.id()
        ])
    );

    // A wider status scope admits the archived bridge too. The default active
    // walk above must not silently cross it, even when the hidden edge is strong.
    let widened = store
        .spread(
            &[Scored {
                id: seed.id(),
                score: 1.0,
            }],
            budget,
            None,
            TraversalScope::new(StatusFilter::ALL),
        )
        .await
        .unwrap();
    let ids: std::collections::HashSet<NodeId> = widened.iter().map(|item| item.id).collect();
    assert!(ids.contains(&active_bridge.id()));
    assert!(ids.contains(&via_active.id()));
    assert!(ids.contains(&archived.id()));
    assert!(ids.contains(&via_archived.id()));
}

#[tokio::test]
async fn contradiction_overlay() {
    let store = CozoStore::new(DIM).unwrap();
    let a = node("claim a", &[]);
    let b = node("claim b", &[]);
    store.put_node(&a).await.unwrap();
    store.put_node(&b).await.unwrap();

    let cold = ColdPath::acquire();
    store
        .observe_contradiction(a.id(), b.id(), 10)
        .await
        .unwrap();
    store
        .observe_contradiction(b.id(), a.id(), 20)
        .await
        .unwrap(); // same pair, swapped order
    let open = store.open_contradictions(cold).await.unwrap();
    assert_eq!(open.len(), 1, "swapped order must collapse to one row");
    assert_eq!(open[0].observations, 2);

    store
        .resolve_contradiction(open[0].between, Resolution::Unresolved)
        .await
        .unwrap();
    let deferred = store.open_contradictions(cold).await.unwrap();
    assert_eq!(deferred.len(), 1, "Unresolved is a deferral, not closure");
    assert_eq!(deferred[0].resolution, Some(Resolution::Unresolved));
    store
        .resolve_contradiction(open[0].between, Resolution::ContextDependent)
        .await
        .unwrap();
    assert!(store.open_contradictions(cold).await.unwrap().is_empty());
    store
        .resolve_contradiction(open[0].between, Resolution::ContextDependent)
        .await
        .unwrap();
    assert!(matches!(
        store
            .resolve_contradiction(open[0].between, Resolution::Superseded)
            .await,
        Err(Error::Conflict(_))
    ));
}

#[tokio::test]
async fn persistent_supersede_is_atomic_and_retry_safe_across_reopen_and_forget() {
    let path = std::env::temp_dir().join(format!("mneme-cozo-supersede-{}.db", Ulid::new()));
    let p = path.to_str().unwrap();
    let winner = node("replacement", &[]);
    let mut loser = node("stale", &[]);
    loser.try_set_confidence(0.8).unwrap();
    let commit = SupersedeCommit::new(winner.id(), loser.id(), 20).unwrap();

    {
        let store = CozoStore::open(p, DIM).unwrap();
        store.put_node(&winner).await.unwrap();
        store.put_node(&loser).await.unwrap();
        store
            .observe_contradiction(winner.id(), loser.id(), 10)
            .await
            .unwrap();
        store
            .resolve_contradiction(commit.pair(), Resolution::Unresolved)
            .await
            .unwrap();
        assert_eq!(
            store
                .open_contradictions(ColdPath::acquire())
                .await
                .unwrap()[0]
                .resolution,
            Some(Resolution::Unresolved)
        );
        assert_eq!(
            store.commit_supersede(&commit).await.unwrap(),
            SupersedeCommitOutcome::Applied
        );
        let after = store.get_node(loser.id()).await.unwrap().unwrap();
        assert_eq!(after.confidence(), 0.8);
        assert_eq!(after.status(), NodeStatus::Archived);
        assert_eq!(
            store
                .get_edge(winner.id(), loser.id())
                .await
                .unwrap()
                .unwrap()
                .kind,
            EdgeKind::Supersedes
        );
        let export = store.export().await.unwrap();
        assert_eq!(export.supersede_commits, vec![commit.record()]);
        let contradiction = export
            .contradictions
            .iter()
            .find(|item| item.between == commit.pair())
            .unwrap();
        assert_eq!(contradiction.observations, 2);
        assert_eq!(contradiction.first_seen, 10);
        assert_eq!(contradiction.last_seen, 20);
        assert_eq!(contradiction.resolution, Some(Resolution::Superseded));
        store.prepare_for_file_move().unwrap();
    }

    {
        let store = reopen_current(&path);
        assert_eq!(
            store.commit_supersede(&commit).await.unwrap(),
            SupersedeCommitOutcome::AlreadyApplied
        );
        assert_eq!(
            store
                .get_node(loser.id())
                .await
                .unwrap()
                .unwrap()
                .confidence(),
            0.8,
            "reopen retry must consult the proof before mutating the loser"
        );
        assert_eq!(
            store.get_node(loser.id()).await.unwrap().unwrap().status(),
            NodeStatus::Archived,
            "reopen retry must preserve the archived lifecycle"
        );
        assert!(matches!(
            store
                .commit_supersede(
                    &SupersedeCommit::new(loser.id(), winner.id(), commit.applied_at + 1).unwrap()
                )
                .await,
            Err(Error::Conflict(_))
        ));

        // The pair proof records a one-time adjudication event, not a standing
        // invariant. Later legal mutations remain authoritative; a same-direction
        // call with a fresh timestamp is still a replay and must not repair them.
        store
            .set_status(loser.id(), NodeStatus::Active)
            .await
            .unwrap();
        let mut later_edge = Edge::new(
            winner.id(),
            loser.id(),
            0.25,
            EdgeKind::Associative,
            commit.applied_at + 1,
        );
        later_edge.anchor = Some(BodySpan::new(1, 2));
        store.put_edge(&later_edge).await.unwrap();
        let later = SupersedeCommit::new(winner.id(), loser.id(), commit.applied_at + 2).unwrap();
        assert_eq!(
            store.commit_supersede(&later).await.unwrap(),
            SupersedeCommitOutcome::AlreadyApplied
        );
        let evolved_loser = store.get_node(loser.id()).await.unwrap().unwrap();
        assert_eq!(evolved_loser.status(), NodeStatus::Active);
        assert_eq!(evolved_loser.confidence(), 0.8);
        let evolved_edge = store
            .get_edge(winner.id(), loser.id())
            .await
            .unwrap()
            .unwrap();
        assert_eq!(evolved_edge.kind, later_edge.kind);
        assert_eq!(evolved_edge.anchor, later_edge.anchor);
        assert_eq!(evolved_edge.weight(), later_edge.weight());

        store.delete_edge(winner.id(), loser.id()).await.unwrap();
        store.delete_node(loser.id()).await.unwrap();
        store.prepare_for_file_move().unwrap();
    }

    let reopened = reopen_current(&path);
    assert_eq!(
        reopened.commit_supersede(&commit).await.unwrap(),
        SupersedeCommitOutcome::AlreadyApplied,
        "forget must retain terminal overlay and direction proof"
    );
    let export = reopened.export().await.unwrap();
    assert_eq!(export.supersede_commits, vec![commit.record()]);
    assert!(export.contradictions.iter().any(|item| {
        item.between == commit.pair() && item.resolution == Some(Resolution::Superseded)
    }));
    drop(reopened);
    let _ = std::fs::remove_file(path);
}

#[tokio::test]
async fn persistent_supersede_terminal_conflict_rolls_back_every_row() {
    let store = CozoStore::new(DIM).unwrap();
    let winner = node("winner", &[]);
    let loser = node("loser", &[]);
    store.put_node(&winner).await.unwrap();
    store.put_node(&loser).await.unwrap();
    let pair = UnorderedPair(winner.id(), loser.id());
    store
        .observe_contradiction(winner.id(), loser.id(), 10)
        .await
        .unwrap();
    store
        .resolve_contradiction(pair, Resolution::ContextDependent)
        .await
        .unwrap();

    assert!(matches!(
        store
            .commit_supersede(&SupersedeCommit::new(winner.id(), loser.id(), 20).unwrap())
            .await,
        Err(Error::Conflict(_))
    ));
    assert!(
        store
            .get_edge(winner.id(), loser.id())
            .await
            .unwrap()
            .is_none()
    );
    let after = store.get_node(loser.id()).await.unwrap().unwrap();
    assert_eq!(after.confidence(), loser.confidence());
    assert_eq!(after.status(), NodeStatus::Active);
    assert!(store.export().await.unwrap().supersede_commits.is_empty());
}

#[tokio::test]
async fn delete_node_persists_selective_overlay_cleanup_across_reopen() {
    let path = std::env::temp_dir().join(format!("mneme-cozo-forget-{}.db", Ulid::new()));
    let p = path.to_str().unwrap();
    let forgotten = node("forgotten endpoint", &[]);
    let winner = node("full merge winner", &[]);
    let open_peer = node("open peer", &[]);
    let resolved_peer = node("resolved peer", &[]);
    let unrelated_a = node("unrelated a", &[]);
    let unrelated_b = node("unrelated b", &[]);
    let full_merge = FullMergeCommit::new(winner.id(), forgotten.id(), 3).unwrap();
    let open_pair = UnorderedPair(forgotten.id(), open_peer.id());
    let resolved_pair = UnorderedPair(forgotten.id(), resolved_peer.id());
    let unrelated_pair = UnorderedPair(unrelated_a.id(), unrelated_b.id());

    {
        let store = CozoStore::open(p, DIM).unwrap();
        for node in [
            &forgotten,
            &winner,
            &open_peer,
            &resolved_peer,
            &unrelated_a,
            &unrelated_b,
        ] {
            store.put_node(node).await.unwrap();
        }

        store
            .observe_merge_candidate(winner.id(), forgotten.id(), 2)
            .await
            .unwrap();
        assert_eq!(
            store.commit_full_merge(&full_merge).await.unwrap(),
            FullMergeCommitOutcome::Applied
        );

        store
            .observe_contradiction(forgotten.id(), open_peer.id(), 4)
            .await
            .unwrap();
        store
            .resolve_contradiction(open_pair, Resolution::Unresolved)
            .await
            .unwrap();
        store
            .observe_merge_candidate(forgotten.id(), open_peer.id(), 5)
            .await
            .unwrap();

        store
            .observe_contradiction(forgotten.id(), resolved_peer.id(), 6)
            .await
            .unwrap();
        store
            .resolve_contradiction(resolved_pair, Resolution::ContextDependent)
            .await
            .unwrap();
        store
            .observe_merge_candidate(forgotten.id(), resolved_peer.id(), 7)
            .await
            .unwrap();
        store
            .resolve_merge_candidate(resolved_pair, MergeResolution::Keep)
            .await
            .unwrap();

        store
            .observe_contradiction(unrelated_a.id(), unrelated_b.id(), 8)
            .await
            .unwrap();
        store
            .observe_merge_candidate(unrelated_a.id(), unrelated_b.id(), 9)
            .await
            .unwrap();

        store.delete_node(forgotten.id()).await.unwrap();
        store.prepare_for_file_move().unwrap();
    }

    let reopened = reopen_current(&path);
    assert!(reopened.get_node(forgotten.id()).await.unwrap().is_none());
    let export = reopened.export().await.unwrap();
    assert!(
        !export
            .contradictions
            .iter()
            .any(|overlay| overlay.between == open_pair)
    );
    assert!(export.contradictions.iter().any(|overlay| {
        overlay.between == resolved_pair && overlay.resolution == Some(Resolution::ContextDependent)
    }));
    assert!(
        export
            .contradictions
            .iter()
            .any(|overlay| { overlay.between == unrelated_pair && overlay.resolution.is_none() })
    );
    assert!(
        !export
            .merges
            .iter()
            .any(|overlay| overlay.between == open_pair)
    );
    assert!(export.merges.iter().any(|overlay| {
        overlay.between == resolved_pair && overlay.resolution == Some(MergeResolution::Keep)
    }));
    assert!(export.merges.iter().any(|overlay| {
        overlay.between == full_merge.pair() && overlay.resolution == Some(MergeResolution::Full)
    }));
    assert!(
        export
            .merges
            .iter()
            .any(|overlay| { overlay.between == unrelated_pair && overlay.resolution.is_none() })
    );
    assert_eq!(export.full_merge_commits, vec![full_merge.record()]);
    assert_eq!(
        reopened.commit_full_merge(&full_merge).await.unwrap(),
        FullMergeCommitOutcome::AlreadyApplied,
        "forgetting the loser must not erase the durable retry proof"
    );

    let cold = ColdPath::acquire();
    assert_eq!(
        reopened
            .open_contradictions(cold)
            .await
            .unwrap()
            .into_iter()
            .map(|overlay| overlay.between)
            .collect::<std::collections::HashSet<_>>(),
        std::collections::HashSet::from([unrelated_pair])
    );
    assert_eq!(
        reopened
            .open_merge_candidates(cold)
            .await
            .unwrap()
            .into_iter()
            .map(|overlay| overlay.between)
            .collect::<std::collections::HashSet<_>>(),
        std::collections::HashSet::from([unrelated_pair])
    );
    drop(reopened);
    let _ = std::fs::remove_file(path);
}

#[tokio::test]
async fn cozo_remote_edges_round_trip() {
    let store = CozoStore::new(DIM).unwrap();
    let from = NodeId(Ulid::new());
    let project_db = Ulid::new();
    let (lo, hi) = (NodeId(Ulid::new()), NodeId(Ulid::new()));

    let empty = store
        .remote_edges_page(from, None, MAX_REMOTE_EDGE_PAGE_SIZE)
        .await
        .unwrap();
    assert!(empty.items.is_empty());
    assert!(empty.next.is_none());

    store
        .put_remote_edge(&RemoteEdge::new(from, project_db, hi, 0.8))
        .await
        .unwrap();
    store
        .put_remote_edge(&RemoteEdge::new(from, project_db, lo, 0.3))
        .await
        .unwrap();
    // Re-link upserts rather than duplicating.
    store
        .put_remote_edge(&RemoteEdge::new(from, project_db, hi, 0.9))
        .await
        .unwrap();

    let edges = store
        .remote_edges_page(from, None, MAX_REMOTE_EDGE_PAGE_SIZE)
        .await
        .unwrap()
        .items;
    assert_eq!(edges.len(), 2, "upsert, not duplicate");
    assert_eq!(edges[0].target, hi, "weight-desc");
    assert_eq!(edges[0].target_db, project_db);
    assert!((edges[0].weight() - 0.9).abs() < 1e-6);

    store
        .delete_remote_edge(from, project_db, hi)
        .await
        .unwrap();
    let left = store
        .remote_edges_page(from, None, MAX_REMOTE_EDGE_PAGE_SIZE)
        .await
        .unwrap()
        .items;
    assert_eq!(left.len(), 1);
    assert_eq!(left[0].target, lo);
}

#[tokio::test]
async fn cozo_remote_edge_keyset_pages_preserve_total_order() {
    let store = CozoStore::new(DIM).unwrap();
    let from = NodeId(Ulid::from(100_000u128));
    let mut expected = Vec::new();
    for offset in 0..130u128 {
        let edge = RemoteEdge::new(
            from,
            Ulid::from(110_000u128 + offset % 5),
            NodeId(Ulid::from(120_000u128 + offset)),
            match offset % 3 {
                0 => 0.8,
                1 => 0.4,
                _ => 0.4,
            },
        );
        store.put_remote_edge(&edge).await.unwrap();
        expected.push(edge);
    }
    expected.sort_by(mneme_core::remote_edge_order);

    let mut actual = Vec::new();
    let mut after = None;
    loop {
        let page = store.remote_edges_page(from, after, 11).await.unwrap();
        assert!(page.items.len() <= 11);
        actual.extend(page.items);
        let Some(next) = page.next else {
            break;
        };
        after = Some(next);
    }
    assert_eq!(actual, expected);
}

#[tokio::test]
async fn snapshot_import_is_lossless_and_preserves_identity() {
    let source = MemStore::new(DIM);
    let identity = fingerprint("test:migration-embedder-v1", DIM);
    source.set_embedding_fingerprint(&identity).unwrap();
    let a = node("migration source a", &["one", "shared"]);
    let b = node("migration source b", &["two", "shared"]);
    let c = node("full merge winner", &["three"]);
    let d = node("full merge loser", &["four"]);
    for node in [&a, &b, &c, &d] {
        source.put_node(node).await.unwrap();
    }
    let av = embed(1.0);
    let bv = embed(-1.0);
    source.upsert(a.id(), &av).await.unwrap();
    source.upsert(b.id(), &bv).await.unwrap();
    let mut edge = Edge::new(a.id(), b.id(), 0.65, EdgeKind::DerivedFrom, 20);
    edge.anchor = Some(BodySpan::new(3, 11));
    source.put_edge(&edge).await.unwrap();
    source
        .observe_contradiction(b.id(), a.id(), 30)
        .await
        .unwrap();
    let contradiction = source
        .open_contradictions(ColdPath::acquire())
        .await
        .unwrap()
        .pop()
        .unwrap();
    source
        .resolve_contradiction(contradiction.between, Resolution::ContextDependent)
        .await
        .unwrap();
    source
        .observe_merge_candidate(a.id(), b.id(), 40)
        .await
        .unwrap();
    let merge = source
        .open_merge_candidates(ColdPath::acquire())
        .await
        .unwrap()
        .pop()
        .unwrap();
    source
        .resolve_merge_candidate(merge.between, MergeResolution::Partial)
        .await
        .unwrap();
    source
        .put_edge(&Edge::from_stored(
            d.id(),
            a.id(),
            EdgeKind::Bridge,
            None,
            0.45,
            41,
            2,
            1,
        ))
        .await
        .unwrap();
    source
        .observe_merge_candidate(c.id(), d.id(), 42)
        .await
        .unwrap();
    source
        .commit_full_merge(&FullMergeCommit::new(c.id(), d.id(), 43).unwrap())
        .await
        .unwrap();
    let remote = RemoteEdge::new(a.id(), Ulid::new(), NodeId(Ulid::new()), 0.77);
    source.put_remote_edge(&remote).await.unwrap();
    let expected = source.export();
    assert_eq!(expected.full_merge_commits.len(), 1);

    let path = std::env::temp_dir().join(format!("mneme-cozo-import-{}.db", Ulid::new()));
    let p = path.to_str().unwrap();
    let mut destination = CozoStore::open(p, DIM).unwrap();
    assert_eq!(destination.import_mem(&source).await.unwrap(), 4);
    assert_eq!(destination.db_id(), source.db_id());
    destination.verify_import(&expected).await.unwrap();
    assert_eq!(
        destination
            .remote_edges_page(a.id(), None, MAX_REMOTE_EDGE_PAGE_SIZE)
            .await
            .unwrap()
            .items,
        vec![remote]
    );
    destination.prepare_for_file_move().unwrap();
    drop(destination);

    let reopened = reopen_current(&path);
    assert_eq!(reopened.db_id(), source.db_id());
    assert_eq!(reopened.embedding_fingerprint().unwrap(), Some(identity));
    reopened.verify_import(&expected).await.unwrap();
    assert_eq!(
        reopened
            .commit_full_merge(&FullMergeCommit::new(c.id(), d.id(), 99).unwrap())
            .await
            .unwrap(),
        FullMergeCommitOutcome::AlreadyApplied,
        "imported retry proof must be checked before the archived loser"
    );
    assert_eq!(
        reopened
            .ann(&av, 1, StatusFilter::ALL)
            .await
            .unwrap()
            .first()
            .map(|hit| hit.id),
        Some(a.id()),
        "the imported vector is queryable after reopen"
    );
    drop(reopened);

    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        assert_eq!(
            std::fs::metadata(&path).unwrap().permissions().mode() & 0o777,
            0o600
        );
    }
    let _ = std::fs::remove_file(path);
}

#[tokio::test]
async fn snapshot_import_rejects_nonempty_destination() {
    let source = MemStore::new(DIM);
    let source_node = node("source", &[]);
    source.put_node(&source_node).await.unwrap();

    let mut destination = CozoStore::new(DIM).unwrap();
    let existing = node("do not overwrite me", &[]);
    destination.put_node(&existing).await.unwrap();
    let error = destination.import_mem(&source).await.unwrap_err();
    assert!(error.to_string().contains("non-empty destination"));
    assert_eq!(
        destination
            .get_node(existing.id())
            .await
            .unwrap()
            .unwrap()
            .summary(),
        "do not overwrite me"
    );
    assert!(
        destination
            .get_node(source_node.id())
            .await
            .unwrap()
            .is_none()
    );
}

#[tokio::test]
async fn failed_vector_import_never_stamps_embedding_identity() {
    let source = MemStore::new(DIM);
    source
        .set_embedding_fingerprint(&fingerprint("test:source-v1", DIM))
        .unwrap();
    let source_node = node("malformed legacy vector", &[]);
    source.put_node(&source_node).await.unwrap();
    source.upsert(source_node.id(), &embed(1.0)).await.unwrap();

    // Loading preserves legacy snapshot bytes rather than silently repairing
    // them. Corrupt one vector length to emulate an interrupted/foreign snapshot
    // and prove the destination never receives the compatibility stamp.
    let mut malformed = source.export();
    malformed.vectors[0].1.pop();
    let path = std::env::temp_dir().join(format!("mneme-bad-vector-{}.json", Ulid::new()));
    std::fs::write(
        &path,
        serde_json::to_vec(&StoreExportEnvelopeV5::new(malformed)).unwrap(),
    )
    .unwrap();
    let malformed = MemStore::load(&path).unwrap();
    let _ = std::fs::remove_file(path);

    let mut destination = CozoStore::new(DIM).unwrap();
    assert!(matches!(
        destination.import_mem(&malformed).await,
        Err(Error::DimMismatch { .. })
    ));
    assert_eq!(destination.embedding_fingerprint().unwrap(), None);
}

#[tokio::test]
async fn malformed_import_fingerprint_is_rejected_before_any_row_mutates() {
    let source = MemStore::new(DIM);
    let source_node = node("fingerprint preflight", &[]);
    source.put_node(&source_node).await.unwrap();
    source.upsert(source_node.id(), &embed(1.0)).await.unwrap();

    let mut future_format = fingerprint("test:future", DIM);
    future_format.format_version += 1;
    let wrong_dimension = fingerprint("test:wrong-dimension", DIM + 1);
    for malformed_fingerprint in [future_format, wrong_dimension] {
        let mut export = source.export();
        export.embedding_fingerprint = Some(malformed_fingerprint);
        let path = std::env::temp_dir().join(format!("mneme-bad-fingerprint-{}.json", Ulid::new()));
        std::fs::write(
            &path,
            serde_json::to_vec(&StoreExportEnvelopeV5::new(export)).unwrap(),
        )
        .unwrap();
        let malformed = MemStore::load(&path).unwrap();
        let _ = std::fs::remove_file(path);

        let mut destination = CozoStore::new(DIM).unwrap();
        assert!(destination.import_mem(&malformed).await.is_err());
        assert!(
            destination
                .all_nodes(ColdPath::acquire())
                .await
                .unwrap()
                .is_empty(),
            "fingerprint validation is an import preflight, not a late commit failure"
        );
        assert_eq!(destination.embedding_fingerprint().unwrap(), None);
    }
}
