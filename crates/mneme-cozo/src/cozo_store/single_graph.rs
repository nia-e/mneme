//! Named predecessor boundary. The original is never opened by a writable
//! constructor: exact admission and a guarded native backup precede all legacy
//! decoding on an operation-owned disposable copy. Source bytes and normalized
//! destination data are distinct commitments; no hash is reconstructed from a
//! subsequently edited memory. Relative body references remain unchanged.

use super::*;
use crate::storage_contract::conventional_unmanaged::admission::{
    CatalogSealGenerationV1, CurrentOpenPermitV1, OpenAdmissionV1,
    close_capture_source_bound_seal_v1, close_concern_source_bound_seal_v1,
    close_episode_context_source_bound_seal_v2, close_episode_source_bound_seal_v1,
    close_single_graph_source_bound_seal_v1, recognize_capture_open_source_v1,
    recognize_concern_open_source_v1, recognize_episode_context_open_source_v2,
    recognize_episode_open_source_v1, recognize_single_graph_open_source_v1,
};
use cozo::{ExistingSqliteSnapshotSource, ManagedSnapshotPolicy};
use mneme_store_path::StoreLease;
use sha2::{Digest, Sha256};
use std::io::Read;
use std::path::{Path, PathBuf};

const MAX_SOURCE_BYTES: u64 = 512 * 1024 * 1024;
const MAX_NODES: usize = 100_000;

#[derive(Clone, Copy)]
enum UpgradeTarget {
    SingleGraphV1,
    ConcernV1,
    EpisodeContextV2,
    TouchstonesV1,
}

impl CozoStore {
    /// Export only a closed capture-v1 or episode-v1 predecessor, normalizing
    /// Candidate membership and adding the exact legacy source codec. The
    /// matching source lease stays borrowed throughout. The 512 MiB main-file
    /// and 100000-node limits are cold-import limits, not normal-read quotas.
    /// No body is opened, moved, or rebased here; callers resolve body paths
    /// against the original source root. Volatile feedback receipts are reset.
    pub async fn export_single_graph_predecessor(
        path: &Path,
        lease: &StoreLease,
    ) -> Result<crate::StoreExport> {
        lease.require_guards(path).map_err(backend)?;
        let original = source_digest(path)?;
        let result = export_copy(path, lease, UpgradeTarget::SingleGraphV1).await;
        // Check even on refusal/failure; an operation error cannot hide source
        // drift. This is a source-byte commitment, not the transformed graph.
        let unchanged = lease.require_guards(path).map_err(backend).and_then(|()| {
            if source_digest(path)? != original {
                return Err(Error::Conflict(
                    "predecessor source changed during detached export".into(),
                ));
            }
            Ok(())
        });
        match (result, unchanged) {
            (Ok(export), Ok(())) => Ok(export),
            (Err(error), Ok(())) => Err(error),
            (_, Err(error)) => Err(error),
        }
    }

    /// Named offline SingleGraphV1 to ConcernV1 predecessor boundary.
    pub async fn export_concern_predecessor(
        path: &Path,
        lease: &StoreLease,
    ) -> Result<crate::StoreExport> {
        lease.require_guards(path).map_err(backend)?;
        let original = source_digest(path)?;
        let result = export_copy(path, lease, UpgradeTarget::ConcernV1).await;
        // Check even on refusal/failure; an operation error cannot hide source
        // drift. This is a source-byte commitment, not the transformed graph.
        let unchanged = lease.require_guards(path).map_err(backend).and_then(|()| {
            if source_digest(path)? != original {
                return Err(Error::Conflict(
                    "predecessor source changed during detached export".into(),
                ));
            }
            Ok(())
        });
        match (result, unchanged) {
            (Ok(export), Ok(())) => Ok(export),
            (Err(error), Ok(())) => Err(error),
            (_, Err(error)) => Err(error),
        }
    }

    /// Named offline ConcernV1 to EpisodeContextV2 predecessor boundary.
    pub async fn export_episode_context_predecessor(
        path: &Path,
        lease: &StoreLease,
    ) -> Result<crate::StoreExport> {
        lease.require_guards(path).map_err(backend)?;
        let original = source_digest(path)?;
        let result = export_copy(path, lease, UpgradeTarget::EpisodeContextV2).await;
        // Check even on refusal/failure; an operation error cannot hide source
        // drift. This is a source-byte commitment, not the transformed graph.
        let unchanged = lease.require_guards(path).map_err(backend).and_then(|()| {
            if source_digest(path)? != original {
                return Err(Error::Conflict(
                    "predecessor source changed during detached export".into(),
                ));
            }
            Ok(())
        });
        match (result, unchanged) {
            (Ok(export), Ok(())) => Ok(export),
            (Err(error), Ok(())) => Err(error),
            (_, Err(error)) => Err(error),
        }
    }

    /// Named offline EpisodeContextV2 to TouchstonesV1 predecessor boundary.
    pub async fn export_touchstones_predecessor(
        path: &Path,
        lease: &StoreLease,
    ) -> Result<crate::StoreExport> {
        lease.require_guards(path).map_err(backend)?;
        let original = source_digest(path)?;
        let result = export_copy(path, lease, UpgradeTarget::TouchstonesV1).await;
        // Check even on refusal/failure; an operation error cannot hide source
        // drift. This is a source-byte commitment, not the transformed graph.
        let unchanged = lease.require_guards(path).map_err(backend).and_then(|()| {
            if source_digest(path)? != original {
                return Err(Error::Conflict(
                    "predecessor source changed during detached export".into(),
                ));
            }
            Ok(())
        });
        match (result, unchanged) {
            (Ok(export), Ok(())) => Ok(export),
            (Err(error), Ok(())) => Err(error),
            (_, Err(error)) => Err(error),
        }
    }
}

fn source_digest(path: &Path) -> Result<[u8; 32]> {
    let metadata = std::fs::symlink_metadata(path).map_err(backend)?;
    if !metadata.file_type().is_file() || metadata.len() > MAX_SOURCE_BYTES {
        return Err(Error::InvalidInput(
            "predecessor must be a regular main file of at most 512 MiB".into(),
        ));
    }
    let mut file = std::fs::File::open(path).map_err(backend)?;
    let mut hash = Sha256::new();
    let mut bytes = 0u64;
    let mut buffer = [0u8; 65536];
    loop {
        let n = file.read(&mut buffer).map_err(backend)?;
        if n == 0 {
            break;
        }
        bytes += n as u64;
        if bytes > MAX_SOURCE_BYTES {
            return Err(Error::InvalidInput(
                "predecessor grew beyond 512 MiB".into(),
            ));
        }
        hash.update(&buffer[..n]);
    }
    if bytes != metadata.len() {
        return Err(Error::Conflict("predecessor changed while hashing".into()));
    }
    Ok(hash.finalize().into())
}

fn admit(
    path: &Path,
    target: UpgradeTarget,
) -> Result<(CurrentOpenPermitV1, ManagedSnapshotPolicy)> {
    if matches!(target, UpgradeTarget::TouchstonesV1) {
        return match recognize_episode_context_open_source_v2(ExistingSqliteSnapshotSource::open(path).map_err(backend)?) {
            Ok(OpenAdmissionV1::Current(permit)) => Ok((permit, ManagedSnapshotPolicy::ConcernV1)),
            _ => Err(Error::InvalidInput("touchstones-v1 upgrade requires exact episode-context-v2 predecessor; current, managed, torn and unknown stores are refused".into())),
        };
    }
    if matches!(target, UpgradeTarget::EpisodeContextV2) {
        return match recognize_concern_open_source_v1(ExistingSqliteSnapshotSource::open(path).map_err(backend)?) {
            Ok(OpenAdmissionV1::Current(permit)) => Ok((permit, ManagedSnapshotPolicy::ConcernV1)),
            _ => Err(Error::InvalidInput("episode-context-v2 upgrade requires exact concern-v1 predecessor; current, managed, torn and unknown stores are refused".into())),
        };
    }
    if matches!(target, UpgradeTarget::ConcernV1) {
        return match recognize_single_graph_open_source_v1(ExistingSqliteSnapshotSource::open(path).map_err(backend)?) {
            Ok(OpenAdmissionV1::Current(permit)) => Ok((permit, ManagedSnapshotPolicy::SingleGraphV1)),
            _ => Err(Error::InvalidInput("concern-v1 upgrade requires exact single-graph-v1 predecessor; current, managed, torn and unknown stores are refused".into())),
        };
    }
    let capture = recognize_capture_open_source_v1(
        ExistingSqliteSnapshotSource::open(path).map_err(backend)?,
    );
    match capture {
        Ok(OpenAdmissionV1::Current(permit)) => Ok((permit, ManagedSnapshotPolicy::V1)),
        _ => match recognize_episode_open_source_v1(ExistingSqliteSnapshotSource::open(path).map_err(backend)?) {
            Ok(OpenAdmissionV1::Current(permit)) => Ok((permit, ManagedSnapshotPolicy::EpisodeV1)),
            _ => Err(Error::InvalidInput("single-graph-upgrade requires exact capture-v1 or episode-v1 predecessor; current, managed, torn, and unknown stores are refused".into())),
        }
    }
}

async fn export_copy(
    path: &Path,
    lease: &StoreLease,
    target: UpgradeTarget,
) -> Result<crate::StoreExport> {
    let (original_permit, policy) = admit(path, target)?;
    let episodic = policy == ManagedSnapshotPolicy::EpisodeV1;
    let single_graph = policy == ManagedSnapshotPolicy::SingleGraphV1;
    let context = matches!(target, UpgradeTarget::TouchstonesV1);
    let concern = policy == ManagedSnapshotPolicy::ConcernV1 && !context;
    let original_dimension = original_permit.vector_dimension();
    drop(original_permit);
    let reader = ExistingSqliteSnapshotSource::open(path)
        .and_then(|s| s.into_managed_reader(policy))
        .map_err(backend)?;
    let source_seal = if context {
        close_episode_context_source_bound_seal_v2(reader)
    } else if concern {
        close_concern_source_bound_seal_v1(reader)
    } else if single_graph {
        close_single_graph_source_bound_seal_v1(reader)
    } else if episodic {
        close_episode_source_bound_seal_v1(reader)
    } else {
        close_capture_source_bound_seal_v1(reader)
    }
    .map_err(backend)?;
    let wanted = if context {
        CatalogSealGenerationV1::EpisodeContextV2
    } else if concern {
        CatalogSealGenerationV1::ConcernV1
    } else if single_graph {
        CatalogSealGenerationV1::SingleGraphV1
    } else if episodic {
        CatalogSealGenerationV1::EpisodeV1
    } else {
        CatalogSealGenerationV1::CaptureV1
    };
    if source_seal.generation() != wanted || source_seal.vector_dimension() != original_dimension {
        return Err(Error::Conflict(
            "predecessor closed-source seal disagrees with admission".into(),
        ));
    }
    lease.require_guards(path).map_err(backend)?;
    let scratch = Scratch::new()?;
    let retained_root = scratch.root.clone();
    let exported = async {
        let copy_lease = StoreLease::acquire(&scratch.path).map_err(backend)?;
        let source = ExistingSqliteSnapshotSource::open(path).map_err(backend)?;
        let copy = source
            .backup_to_new_snapshot_source_v1(&scratch.path)
            .map_err(backend)?;
        drop(source);
        let admitted = if context {
            recognize_episode_context_open_source_v2(copy)
        } else if concern {
            recognize_concern_open_source_v1(copy)
        } else if single_graph {
            recognize_single_graph_open_source_v1(copy)
        } else if episodic {
            recognize_episode_open_source_v1(copy)
        } else {
            recognize_capture_open_source_v1(copy)
        }
        .map_err(|_| Error::Conflict("private predecessor backup failed exact admission".into()))?;
        let OpenAdmissionV1::Current(permit) = admitted else {
            return Err(Error::Conflict(
                "private predecessor backup lost current codec".into(),
            ));
        };
        let admission = permit
            .into_existing_open_admission_v1(&scratch.path)
            .map_err(|_| Error::Conflict("private predecessor copy identity changed".into()))?;
        let (_, dim, _, runtime_permit) = admission.into_runtime_parts();
        copy_lease.require_guards(&scratch.path).map_err(backend)?;
        let database = DbInstance::open_existing_sqlite(runtime_permit).map_err(backend)?;
        let mut store = fresh_current::store_over_staged_database(database, dim, Ulid::nil());
        store.db_id = store
            .read_canonical_database_id()?
            .ok_or_else(|| Error::InvalidInput("predecessor database identity missing".into()))?;
        let result = async {
            let mut nodes = Vec::new();
            let mut after = None;
            loop {
                let page = store.scan_primary_key(
                    "node",
                    PrimaryKeyScan {
                        prefix: vec![],
                        lower: after
                            .take()
                            .map_or(PrimaryKeyScanBound::Unbounded, |id: String| {
                                PrimaryKeyScanBound::Excluded(vec![dv_str(&id)])
                            }),
                        upper: PrimaryKeyScanBound::Unbounded,
                        direction: PrimaryKeyScanDirection::Ascending,
                        limit: 64,
                    },
                )?;
                if page.rows.rows.is_empty() {
                    break;
                }
                for row in &page.rows.rows {
                    let id = want_str(&row[0])?;
                    let data = want_str(&row[1])?;
                    if data.len() > crate::canonical_node_contract::MAX_CANONICAL_NODE_JSON_BYTES {
                        return Err(Error::InvalidInput(
                            "legacy canonical node exceeds byte bound".into(),
                        ));
                    }
                    let raw: serde_json::Value = serde_json::from_str(data)
                        .map_err(|error| Error::InvalidInput(error.to_string()))?;
                    if !context {
                        crate::validate_pre_context_node_value(&raw)?;
                    }
                    let (node, old_status) = if single_graph || concern || context {
                        let node = decode_canonical_node_row(row)?;
                        let status = status_str(node.status());
                        (node, status)
                    } else {
                        crate::decode_legacy_node_v1(data)?
                    };
                    if node.id().0.to_string() != id || want_str(&row[2])? != old_status {
                        return Err(Error::InvalidInput(
                            "legacy canonical keyed ID/status mismatch".into(),
                        ));
                    }
                    if nodes.len() == MAX_NODES {
                        return Err(Error::CapacityExceeded {
                            resource: "single-graph predecessor nodes",
                            limit: MAX_NODES,
                        });
                    }
                    after = Some(id.to_owned());
                    nodes.push(node);
                }
            }
            let mut export = store.export_with_nodes(nodes).await?;
            // A new host cannot inherit old process-scoped receipt authority.
            export.feedback_retries.clear();
            crate::MemStore::from_export(export.clone())?;
            Ok(export)
        }
        .await;
        let closed = store.prepare_for_file_move();
        drop(store);
        copy_lease.require_guards(&scratch.path).map_err(backend)?;
        drop(copy_lease);
        drop(source_seal);
        // On any failed close retain the private operation directory for inspection;
        // never delete a file while a runtime may still own it.
        closed?;
        scratch.cleanup()?;
        result
    }
    .await;
    exported.map_err(|error| backend_str(format!(
        "{error}; operation-owned predecessor copy residue may remain at {} (original source is not this path)", retained_root.display()
    )))
}

struct Scratch {
    root: PathBuf,
    path: PathBuf,
}
impl Scratch {
    fn new() -> Result<Self> {
        let root = std::env::temp_dir()
            .canonicalize()
            .map_err(backend)?
            .join(format!("mneme-single-graph-{}", Ulid::new()));
        #[cfg(unix)]
        {
            use std::os::unix::fs::DirBuilderExt;
            std::fs::DirBuilder::new()
                .mode(0o700)
                .create(&root)
                .map_err(backend)?;
        }
        #[cfg(not(unix))]
        std::fs::create_dir(&root).map_err(backend)?;
        Ok(Self {
            path: root.join("source-copy.db"),
            root,
        })
    }
    fn cleanup(self) -> Result<()> {
        // Only explicit owned leaves; unexpected residue makes remove_dir fail
        // and is retained, never recursively removed.
        std::fs::remove_file(&self.path).map_err(backend)?;
        // The process lease owns a separate persistent sibling lockfile.
        let lock = mneme_store_path::store_lock_path(&self.path).map_err(backend)?;
        if lock.exists() {
            std::fs::remove_file(lock).map_err(backend)?;
        }
        std::fs::remove_dir(&self.root).map_err(backend)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use mneme_core::episode::{EpisodeFacet, EpisodeTime, OccurrenceSpan};
    use mneme_core::ports::CaptureCommitOutcome;
    use mneme_core::{BodyRef, CaptureReplayProof, CaptureRequestCodec, CaptureSource};

    fn source(codec: CaptureRequestCodec) -> CaptureSource {
        CaptureSource::new_with_codec(
            "single-graph-test",
            "captured",
            "file:///original/source",
            Some("source-session"),
            Some("revision"),
            [7; 32],
            codec,
        )
        .unwrap()
    }

    fn semantic(capture: CaptureSource) -> Node {
        Node::try_new(
            capture.node_id(),
            "Edited after original capture",
            BodyRef::new("inline://source-body").unwrap(),
            ["core", "old"],
            Provenance::External { source: capture },
            0.81,
            0.67,
            NodeStatus::Active,
            42,
        )
        .unwrap()
    }

    // Synthetic exact-old-shape fixture, not a claim of genuine old-binary
    // production. Separate retained predecessor artifacts qualify that boundary.
    fn predecessor(path: &Path, episodic: bool, malformed_node: bool) {
        let db = DbInstance::new("sqlite", path, "").unwrap();
        let db_id = DatabaseId::new(Ulid::from(91_u128)).unwrap();
        if episodic {
            creation::stage_episode_schema(&db, 4, db_id).unwrap();
        } else {
            creation::stage_capture_schema(&db, 4, db_id).unwrap();
        }
        let mut node = semantic(source(CaptureRequestCodec::CaptureV1));
        if episodic {
            let capture = source(CaptureRequestCodec::EpisodeV1);
            node = Node::try_new(
                capture.node_id(),
                "A historical episode",
                BodyRef::new("inline://source-body").unwrap(),
                ["old"],
                Provenance::External { source: capture },
                0.5,
                0.5,
                NodeStatus::Active,
                42,
            )
            .unwrap();
            let episode_id = node.id();
            node = node
                .with_episode(
                    EpisodeFacet::initial(
                        episode_id,
                        OccurrenceSpan::Unknown,
                        None,
                        EpisodeTime::new(42).unwrap(),
                    )
                    .unwrap(),
                )
                .unwrap();
        }
        let mut json = serde_json::to_value(&node).unwrap();
        json["provenance"]["External"]["source"]
            .as_object_mut()
            .unwrap()
            .remove("request_codec");
        if !episodic {
            json["status"] = serde_json::json!({"Candidate":{"use_count":9}});
        }
        if malformed_node {
            json["summary"] = serde_json::json!("");
        }
        let status = if episodic { "active" } else { "candidate" };
        let mut params = BTreeMap::new();
        params.insert("id".into(), dv_str(&node.id().0.to_string()));
        params.insert(
            "data".into(),
            dv_str(&serde_json::to_string(&json).unwrap()),
        );
        params.insert("status".into(), dv_str(status));
        params.insert("summary".into(), dv_str(node.summary()));
        params.insert("hash".into(), dv_int(stable_tag_sample_hash(node.id())));
        db.run_script(
            "?[id,data,status] <- [[$id,$data,$status]] :put node {id => data,status}",
            params.clone(),
            cozo::ScriptMutability::Mutable,
        )
        .unwrap();
        if !episodic {
            db.run_script("?[id,summary,status] <- [[$id,$summary,$status]] :put node_search {id => summary,status}",params.clone(),cozo::ScriptMutability::Mutable).unwrap();
            db.run_script("?[tag,status,sample_hash,id] <- [['core',$status,$hash,$id],['old',$status,$hash,$id]] :put node_tag_v2 {tag,status,sample_hash,id}",params.clone(),cozo::ScriptMutability::Mutable).unwrap();
        }
        params.insert(
            "vector_status".into(),
            dv_str(if episodic { "episode" } else { status }),
        );
        db.run_script("?[id,e,status] := id=$id,e=vec([1.0,0.0,0.0,0.0]),status=$vector_status :put node_vec {id=>e,status}",params,cozo::ScriptMutability::Mutable).unwrap();
        db.prepare_sqlite_for_file_move().unwrap();
        drop(db);
    }

    #[tokio::test]
    async fn detached_capture_normalizes_membership_preserves_source_and_replays() {
        let scratch = Scratch::new().unwrap();
        predecessor(&scratch.path, false, false);
        let before = source_digest(&scratch.path).unwrap();
        let lease = StoreLease::acquire(&scratch.path).unwrap();
        assert!(CozoStore::require_existing_current(&scratch.path, &lease).is_err());
        let export = CozoStore::export_single_graph_predecessor(&scratch.path, &lease)
            .await
            .unwrap();
        assert_eq!(source_digest(&scratch.path).unwrap(), before);
        assert_eq!(export.db_id, Ulid::from(91_u128));
        assert_eq!(export.nodes.len(), 1);
        let stored = &export.nodes[0];
        assert_eq!(stored.status(), NodeStatus::Active);
        assert_eq!(stored.confidence(), 0.67);
        assert_eq!(stored.stability(), 0.81);
        let Provenance::External {
            source: stored_source,
        } = stored.provenance()
        else {
            panic!()
        };
        assert_eq!(
            stored_source.request_codec(),
            CaptureRequestCodec::CaptureV1
        );
        assert_eq!(stored_source.request_digest(), [7; 32]);
        let destination = scratch.root.join("destination.db");
        CozoStore::materialize_concern(
            &destination,
            Ulid::new(),
            &crate::MemStore::from_export(export).unwrap(),
        )
        .await
        .unwrap();
        let target_lease = StoreLease::acquire(&destination).unwrap();
        let target = CozoStore::open_leased_concern(&destination, Arc::new(target_lease)).unwrap();
        assert!(!target.relation_exists("node_vec:candidate_idx").unwrap());
        assert!(!target.relation_exists("node_search:candidate_fts").unwrap());
        let mut value = serde_json::to_value(source(CaptureRequestCodec::CaptureV2)).unwrap();
        value["request_digest"] = serde_json::json!(vec![8; 32]);
        let incoming_source: CaptureSource = serde_json::from_value(value).unwrap();
        let proof =
            CaptureReplayProof::semantic(incoming_source.clone(), [7; 32], [6; 32]).unwrap();
        let incoming = semantic(incoming_source);
        assert_eq!(
            target.lookup_capture(&proof).await.unwrap(),
            Some(incoming.id())
        );
        assert_eq!(
            target
                .commit_capture(&incoming, &[1.0, 0.0, 0.0, 0.0], &proof)
                .await
                .unwrap(),
            CaptureCommitOutcome::AlreadyApplied
        );
        assert_eq!(
            target
                .get_node(incoming.id())
                .await
                .unwrap()
                .unwrap()
                .summary(),
            "Edited after original capture"
        );
        target.prepare_for_file_move().unwrap();
        drop(target);
        drop(lease);
        std::fs::remove_dir_all(&scratch.root).unwrap();
    }

    #[tokio::test]
    async fn episode_predecessor_keeps_facet_codec_and_source_untouched() {
        let scratch = Scratch::new().unwrap();
        predecessor(&scratch.path, true, false);
        let before = source_digest(&scratch.path).unwrap();
        let lease = StoreLease::acquire(&scratch.path).unwrap();
        let export = CozoStore::export_single_graph_predecessor(&scratch.path, &lease)
            .await
            .unwrap();
        assert_eq!(source_digest(&scratch.path).unwrap(), before);
        assert!(!export.nodes[0].is_semantic());
        let Provenance::External { source } = export.nodes[0].provenance() else {
            panic!()
        };
        assert_eq!(source.request_codec(), CaptureRequestCodec::EpisodeV1);
        drop(lease);
        std::fs::remove_dir_all(&scratch.root).unwrap();
    }

    #[tokio::test]
    async fn malformed_legacy_node_refuses_without_changing_source() {
        let scratch = Scratch::new().unwrap();
        predecessor(&scratch.path, false, true);
        let before = source_digest(&scratch.path).unwrap();
        let lease = StoreLease::acquire(&scratch.path).unwrap();
        assert!(
            CozoStore::export_single_graph_predecessor(&scratch.path, &lease)
                .await
                .is_err()
        );
        assert_eq!(source_digest(&scratch.path).unwrap(), before);
        drop(lease);
        std::fs::remove_dir_all(&scratch.root).unwrap();
    }
}

#[cfg(test)]
mod retained_fixture_qualification {
    use super::*;

    fn copy_fixture_bodies(source: &Path, target: &Path) {
        std::fs::create_dir(target).unwrap();
        let entries: Vec<_> = std::fs::read_dir(source).unwrap().collect();
        assert!(entries.len() <= 64);
        for entry in entries {
            let entry = entry.unwrap();
            let metadata = entry.path().symlink_metadata().unwrap();
            assert!(metadata.is_file() && metadata.len() <= 1024 * 1024);
            std::fs::copy(entry.path(), target.join(entry.file_name())).unwrap();
        }
    }

    /// Opt-in exact retained synthetic artifacts only; no user/session/store
    /// discovery. Supply an absolute fixture directory explicitly, then run with
    /// --ignored after verifying these pins.
    #[tokio::test]
    #[ignore = "requires MNEME_SINGLE_GRAPH_PREDECESSOR_FIXTURES with pinned synthetic artifacts"]
    async fn genuine_capture_and_episode_predecessors_materialize_without_source_changes() {
        let root = PathBuf::from(
            std::env::var_os("MNEME_SINGLE_GRAPH_PREDECESSOR_FIXTURES").expect(
                "supply an absolute directory containing the pinned synthetic predecessors",
            ),
        );
        assert!(root.is_absolute(), "fixture directory must be absolute");
        let retained_root =
            std::env::var_os("MNEME_SINGLE_GRAPH_QUALIFICATION_OUTPUT").map(PathBuf::from);
        if let Some(retained) = &retained_root {
            assert!(
                retained.is_absolute(),
                "explicit qualification output must be absolute"
            );
            std::fs::create_dir(retained).expect("qualification output must be absent");
        }
        for (name, expected) in [
            (
                "capture-v1.db",
                "d652091e6df1f015a2b163cdbd7541d6156bd14670803f4bedf9a6c97215cc3d",
            ),
            (
                "capture-v1-feedback-edited.db",
                "80b270b183deef4feed6fcc56a15d9d288f46e1378e68974b3cf873e21fb17ef",
            ),
            (
                "episode-v1.db",
                "f19c345d59d69fe538bc07dfdcb08be5c1723c74bc426ccd8600ff741d0aa03a",
            ),
        ] {
            let original = root.join(name);
            let hash = source_digest(&original).unwrap();
            assert_eq!(
                hash.iter().map(|b| format!("{b:02x}")).collect::<String>(),
                expected
            );
            let scratch = Scratch::new().unwrap();
            std::fs::copy(&original, &scratch.path).unwrap();
            copy_fixture_bodies(
                &original.with_extension("bodies"),
                &scratch.path.with_extension("bodies"),
            );
            let lease = StoreLease::acquire(&scratch.path).unwrap();
            assert!(CozoStore::require_existing_current(&scratch.path, &lease).is_err());
            let export = CozoStore::export_single_graph_predecessor(&scratch.path, &lease)
                .await
                .unwrap();
            let old_digest = source_digest(&scratch.path).unwrap();
            assert_eq!(old_digest, hash);
            assert!(
                export
                    .nodes
                    .iter()
                    .all(|n| matches!(n.status(), NodeStatus::Active | NodeStatus::Archived))
            );
            assert!(
                export
                    .nodes
                    .iter()
                    .filter_map(|n| match n.provenance() {
                        Provenance::External { source } => Some((n, source)),
                        _ => None,
                    })
                    .all(|(n, s)| s.request_codec()
                        == if n.is_semantic() {
                            mneme_core::CaptureRequestCodec::CaptureV1
                        } else {
                            mneme_core::CaptureRequestCodec::EpisodeV1
                        })
            );
            let expected_export = export.clone();
            let destination = scratch.root.join("successor.db");
            CozoStore::materialize_single_graph(
                &destination,
                Ulid::new(),
                &crate::MemStore::from_export(export).unwrap(),
            )
            .await
            .unwrap();
            let target_lease = StoreLease::acquire(&destination).unwrap();
            assert!(CozoStore::require_existing_current(&destination, &target_lease).is_err());
            let historical_hash = source_digest(&destination).unwrap();
            let historical_export =
                CozoStore::export_concern_predecessor(&destination, &target_lease)
                    .await
                    .unwrap();
            assert!(historical_export.concerns.is_empty());
            assert_eq!(
                canonical_export_value(historical_export.clone()).unwrap(),
                canonical_export_value(expected_export.clone()).unwrap()
            );
            assert_eq!(source_digest(&destination).unwrap(), historical_hash);
            drop(target_lease);
            let concern_destination = scratch.root.join("concern.db");
            CozoStore::materialize_concern(
                &concern_destination,
                Ulid::new(),
                &crate::MemStore::from_export(historical_export).unwrap(),
            )
            .await
            .unwrap();
            let concern_lease = StoreLease::acquire(&concern_destination).unwrap();
            let target =
                CozoStore::open_leased_concern(&concern_destination, Arc::new(concern_lease))
                    .unwrap();
            target.verify_import(&expected_export).await.unwrap();
            assert_eq!(target.db_id(), expected_export.db_id);
            target.prepare_for_file_move().unwrap();
            drop(target);
            drop(lease);
            assert_eq!(source_digest(&original).unwrap(), hash);
            if let Some(retained) = &retained_root {
                let case = retained.join(name.trim_end_matches(".db"));
                std::fs::create_dir(&case).unwrap();
                let copied_target = case.join("successor.db");
                std::fs::copy(&destination, &copied_target).unwrap();
                copy_fixture_bodies(
                    &original.with_extension("bodies"),
                    &copied_target.with_extension("bodies"),
                );
                let json = case.join("successor.json");
                crate::MemStore::from_export(expected_export.clone())
                    .unwrap()
                    .save_single_graph_v2(&json)
                    .unwrap();
                let hex = |h: [u8; 32]| h.iter().map(|b| format!("{b:02x}")).collect::<String>();
                std::fs::write(case.join("receipt.json"), serde_json::to_vec_pretty(&serde_json::json!({
                    "schema":"mneme.single-graph.retained-fixture-qualification.v1",
                    "source_name":name, "source_sha256":hex(hash), "source_unchanged":true,
                    "successor_sha256":hex(source_digest(&copied_target).unwrap()),
                    "json_sha256":hex(source_digest(&json).unwrap()),
                    "producer":"current mneme-cozo Rust unit-test artifact; materialize_single_graph and MemStore::save_single_graph_v2; named export then materialize_concern independently verified",
                    "historical_generation":"single-graph-v1", "json_generation":"mneme.store-export.v2", "concern_successor_verified":true,
                    "db_id":expected_export.db_id.to_string(), "nodes":expected_export.nodes.len(),
                    "normal_predecessor_open_refused":true, "complete_export_verified":true
                })).unwrap()).unwrap();
            }
            std::fs::remove_dir_all(&scratch.root).unwrap();
        }
    }
    #[tokio::test]
    #[ignore = "requires MNEME_SINGLE_GRAPH_V1_FIXTURE with a synthetic predecessor"]
    async fn genuine_single_graph_predecessor_upgrades_to_concern_without_source_changes() {
        let original = PathBuf::from(
            std::env::var_os("MNEME_SINGLE_GRAPH_V1_FIXTURE")
                .expect("supply an absolute synthetic SingleGraphV1 fixture path"),
        );
        assert!(original.is_absolute(), "fixture path must be absolute");
        let original_hash = source_digest(&original).unwrap();
        let scratch = Scratch::new().unwrap();
        std::fs::copy(&original, &scratch.path).unwrap();
        let lease = StoreLease::acquire(&scratch.path).unwrap();
        let copy_hash = source_digest(&scratch.path).unwrap();
        assert_eq!(copy_hash, original_hash);
        let export = CozoStore::export_concern_predecessor(&scratch.path, &lease)
            .await
            .unwrap();
        assert!(export.concerns.is_empty());
        assert_eq!(source_digest(&scratch.path).unwrap(), copy_hash);
        assert!(
            CozoStore::export_single_graph_predecessor(&scratch.path, &lease)
                .await
                .is_err()
        );
        drop(lease);
        let expected = export.clone();
        let target = scratch.root.join("concern.db");
        CozoStore::materialize_concern(
            &target,
            Ulid::new(),
            &crate::MemStore::from_export(export).unwrap(),
        )
        .await
        .unwrap();
        let target_lease = StoreLease::acquire(&target).unwrap();
        let current = CozoStore::open_leased_concern(&target, Arc::new(target_lease)).unwrap();
        current.verify_import(&expected).await.unwrap();
        assert!(current.relation_exists("concern").unwrap());
        assert!(current.relation_exists("concern:by_hi").unwrap());
        current.prepare_for_file_move().unwrap();
        drop(current);
        assert_eq!(source_digest(&original).unwrap(), original_hash);
        std::fs::remove_file(&target).unwrap();
        std::fs::remove_file(mneme_store_path::store_lock_path(&target).unwrap()).unwrap();
        scratch.cleanup().unwrap();
    }
    #[tokio::test]
    async fn concern_predecessor_rejects_raw_context_presence_and_new_codec_without_touching_source()
     {
        use mneme_core::episode::{
            EpisodeCommit, EpisodeFacet, EpisodeStore, EpisodeTime, EpisodeWriteExpectation,
            OccurrenceSpan,
        };
        use mneme_core::{BodyRef, CaptureSource};
        let source = crate::MemStore::new(4);
        let capture = CaptureSource::new_with_codec(
            "smuggling",
            "old-edition",
            "file:///synthetic",
            None::<&str>,
            None::<&str>,
            [31; 32],
            mneme_core::CaptureRequestCodec::EpisodeV1,
        )
        .unwrap();
        let node = Node::try_new(
            capture.node_id(),
            "An old account with unknown original context",
            BodyRef::new("inline://old-edition").unwrap(),
            ["scene"],
            Provenance::External { source: capture },
            0.5,
            0.5,
            NodeStatus::Active,
            10,
        )
        .unwrap();
        let episode_id = node.id();
        let node = node
            .with_episode(
                EpisodeFacet::initial(
                    episode_id,
                    OccurrenceSpan::Unknown,
                    None,
                    EpisodeTime::new(10).unwrap(),
                )
                .unwrap(),
            )
            .unwrap();
        source
            .commit_episode(EpisodeCommit {
                node: &node,
                embedding: &[1., 0., 0., 0.],
                links: &[],
                expectation: EpisodeWriteExpectation::NewRoot,
            })
            .await
            .unwrap();
        for attack in [
            serde_json::Value::Null,
            serde_json::json!([]),
            serde_json::json!([{"namespace":"session","key":"pi"}]),
            serde_json::json!("new-codec"),
        ] {
            let target = Scratch::new().unwrap();
            // Scratch reserves its directory, not its database file.
            CozoStore::materialize_concern(&target.path, Ulid::new(), &source)
                .await
                .unwrap();
            let lease = Arc::new(StoreLease::acquire(&target.path).unwrap());
            let store = CozoStore::open_leased_concern(&target.path, lease.clone()).unwrap();
            let mut raw = serde_json::to_value(&node).unwrap();
            if attack == serde_json::json!("new-codec") {
                raw["provenance"]["External"]["source"]["request_codec"] =
                    serde_json::json!("episode_v2");
            } else {
                raw["memory_kind"]["episode"]["occurrence_contexts"] = attack;
            }
            let params = BTreeMap::from([
                ("id".into(), dv_str(&node.id().0.to_string())),
                ("data".into(), dv_str(&serde_json::to_string(&raw).unwrap())),
            ]);
            store.run("?[id,data,status] := id=$id,data=$data,*node{id,status} :put node {id=>data,status}", params, true).unwrap();
            store.prepare_for_file_move().unwrap();
            drop(store);
            let before = source_digest(&target.path).unwrap();
            let error = CozoStore::export_episode_context_predecessor(&target.path, &lease)
                .await
                .unwrap_err();
            assert!(
                error.to_string().contains("pre-context")
                    || error.to_string().contains("occurrence_contexts")
                    || error.to_string().contains("episode_v2"),
                "{error}"
            );
            assert_eq!(source_digest(&target.path).unwrap(), before);
            drop(lease);
        }
    }
}
