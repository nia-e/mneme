//! Explicit, detached generation upgrade. The source is never replaced or
//! activated here; the operator switches owners only after inspecting success.

use std::path::{Path, PathBuf};

use clap::{Args, ValueEnum};

use crate::AnyErr;

#[derive(Args)]
pub(crate) struct SingleGraphUpgradeArgs {
    /// Predecessor storage format; never guessed by a normal database opener.
    #[arg(long, value_enum)]
    pub(crate) backend: PredecessorBackend,
    /// Explicit successor: single-graph-v1 admits capture/episode or flat v1;
    /// concern-v1 admits only single-graph-v1 SQLite or v2 JSON;
    /// episode-context-v2 admits only concern-v1 SQLite or v3 JSON;
    /// touchstones-v1 admits only episode-context-v2 SQLite or v4 JSON.
    #[arg(long, value_enum, default_value = "single-graph-v1")]
    pub(crate) target_generation: TargetGeneration,
    /// Absolute absent destination database in an existing directory. The
    /// predecessor remains intact and is never activated automatically.
    #[arg(long, value_name = "ABSENT_PATH")]
    pub(crate) output: PathBuf,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, ValueEnum)]
pub(crate) enum TargetGeneration {
    SingleGraphV1,
    ConcernV1,
    EpisodeContextV2,
    TouchstonesV1,
}

impl TargetGeneration {
    fn as_str(self) -> &'static str {
        match self {
            Self::SingleGraphV1 => "single-graph-v1",
            Self::ConcernV1 => "concern-v1",
            Self::EpisodeContextV2 => "episode-context-v2",
            Self::TouchstonesV1 => "touchstones-v1",
        }
    }
}

#[derive(Clone, Copy, Debug, ValueEnum)]
pub(crate) enum PredecessorBackend {
    Sqlite,
    Json,
}

pub(crate) async fn run_single_graph(
    source: &Path,
    args: &SingleGraphUpgradeArgs,
    json_output: bool,
) -> Result<(), AnyErr> {
    persistent::run(
        source,
        &args.output,
        args.backend,
        args.target_generation,
        json_output,
    )
    .await
}

/// Keep both offline capabilities alive until the caller renders acknowledgement.
pub(crate) struct JsonTagFormatReceipt {
    value: serde_json::Value,
    _source_lease: std::sync::Arc<mneme_store_path::StoreLease>,
    _target_lease: Option<std::sync::Arc<mneme_store_path::StoreLease>>,
}
impl std::ops::Deref for JsonTagFormatReceipt {
    type Target = serde_json::Value;
    fn deref(&self) -> &Self::Target {
        &self.value
    }
}
impl std::fmt::Debug for JsonTagFormatReceipt {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        self.value.fmt(f)
    }
}

pub(crate) fn tag_format_failure_report(error: &AnyErr) -> Option<&serde_json::Value> {
    persistent::tag_failure_report(error)
}

pub(crate) async fn run_tag_format_json(
    source: &Path,
    output: Option<&Path>,
    expected_db_id: ulid::Ulid,
    options: &mneme_cozo::tag_format_migration::TagFormatMigrationOptions,
    expected_plan: Option<&str>,
) -> Result<JsonTagFormatReceipt, AnyErr> {
    persistent::tag_format_json(source, output, expected_db_id, options, expected_plan).await
}

mod persistent {
    use std::collections::BTreeSet;
    use std::fmt;
    use std::fs::{self, File, OpenOptions};
    use std::io::{Read, Write};
    use std::path::Component;
    use std::sync::Arc;

    use mneme_body::FsStore;
    use mneme_cozo::MemStore;
    #[cfg(feature = "cozo")]
    use mneme_cozo::{CozoStore, FreshCurrentMaterializationErrorV1};
    use mneme_store_path::StoreLease;
    use serde::Serialize;
    use serde_json::{Value, json};
    use sha2::{Digest, Sha256};
    use ulid::Ulid;

    use super::*;

    const MAX_PAYLOAD_BYTES: u64 = 512 * 1024 * 1024;
    const MANIFEST: &str = ".single-graph-upgrade-manifest.json";

    // The error owns the materializer's typed capability until the CLI has
    // reported the exact publication state. Never flatten it into a string or
    // invoke the unpublished-outer-discard API: this target has no such outer.
    struct UpgradeFailure {
        report: Value,
        source: AnyErr,
        _source_lease: Option<Arc<StoreLease>>,
        _target_lease: Option<Arc<StoreLease>>,
    }
    impl fmt::Display for UpgradeFailure {
        fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
            write!(
                f,
                "single-graph-upgrade failed in {}: {}; preserve the output and body paths for inspection; the source was not replaced",
                self.report["phase"], self.source
            )
        }
    }
    impl fmt::Debug for UpgradeFailure {
        fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
            fmt::Display::fmt(self, f)
        }
    }
    impl std::error::Error for UpgradeFailure {
        fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
            Some(self.source.as_ref())
        }
    }

    struct Progress {
        source: PathBuf,
        target: PathBuf,
        operation_id: Ulid,
        phase: &'static str,
        db_id: Option<Ulid>,
        source_lease: Option<Arc<StoreLease>>,
        target_lease: Option<Arc<StoreLease>>,
        bodies_created: bool,
        target_published: bool,
        backend: PredecessorBackend,
        target_generation: TargetGeneration,
        tag_export: Option<mneme_cozo::StoreExport>,
        tag_plan: Option<Value>,
    }
    impl Progress {
        fn report(&self, status: &str) -> Value {
            let mut report = json!({
                "status": status, "phase": self.phase,
                "target_generation": self.target_generation.as_str(),
                "source": self.source, "output": self.target,
                "bodies": self.target.with_extension("bodies"),
                "operation_id": self.operation_id.to_string(),
                "db_id": self.db_id.map(|id| id.to_string()),
                "source_replaced": false, "activated": false,
                "bodies_created": self.bodies_created,
                "backend": match self.backend { PredecessorBackend::Sqlite => "sqlite", PredecessorBackend::Json => "json" },
                "stage": self.target.parent().map(|parent| parent.join(format!(".single-graph-upgrade-{}", self.operation_id))),
            });
            if self.target_generation == TargetGeneration::EpisodeContextV2 {
                report["binding_codec_transition"] = json!({
                    "scope": "requested_target_generation",
                    "publication_verified": self.phase == "complete",
                    "content_fingerprint": {"from": "mneme.routing-content.v1", "to": mneme_core::ports::ROUTING_CONTENT_FINGERPRINT_CODEC},
                    "edge_fingerprint": {"from": "mneme.routing-edge.v1", "to": mneme_core::ports::ROUTING_EDGE_FINGERPRINT_CODEC},
                    "current_encoding": "stable_canonical_bytes_not_debug",
                    "historical_meaning_hashes": "preserved_without_rebinding",
                    "legacy_automatic_applicability": "stale_until_fresh_grounded_evidence",
                    "accumulated_edge_learning": "preserved",
                    "concern_cache": "fresh_notice_may_replace_latest_cache",
                    "historical_source": "retained_without_replacement"
                });
            }
            report
        }
    }

    pub(super) async fn run(
        source: &Path,
        target: &Path,
        backend: PredecessorBackend,
        target_generation: TargetGeneration,
        json_output: bool,
    ) -> Result<(), AnyErr> {
        let mut progress = Progress {
            source: source.to_owned(),
            target: target.to_owned(),
            operation_id: Ulid::new(),
            phase: "preflight",
            db_id: None,
            source_lease: None,
            target_lease: None,
            bodies_created: false,
            target_published: false,
            backend,
            target_generation,
            tag_export: None,
            tag_plan: None,
        };
        match upgrade(&mut progress, backend).await {
            Ok(report) => {
                if json_output {
                    crate::print_json(&report);
                } else {
                    println!(
                        "created {} copy {} (database {}); source {} retained; not activated",
                        progress.target_generation.as_str(),
                        progress.target.display(),
                        progress.db_id.unwrap(),
                        progress.source.display()
                    );
                    println!(
                        "copied and verified {} bodies; volatile feedback receipts were reset",
                        report["body_files"]
                    );
                    if progress.target_generation == TargetGeneration::EpisodeContextV2 {
                        println!(
                            "Canonical v2 fingerprints; old judgments need fresh evidence. Graph learning and original records retained."
                        );
                    }
                    if report["non_fs_body_refs_retained_unfetched"]
                        .as_u64()
                        .is_some_and(|count| count > 0)
                    {
                        println!(
                            "retained {} non-fs body references unchanged; their content was not fetched or verified",
                            report["non_fs_body_refs_retained_unfetched"]
                        );
                    }
                }
                Ok(())
            }
            Err(source) => {
                let mut report = progress.report("failed");
                report["error"] = json!(source.to_string());
                report["recovery"] = json!(
                    "The source was not replaced. Preserve and inspect any destination, its body directory and operation residue. Do not activate a failed result or blindly retry against the same target; use a new absent output after resolving the reported cause."
                );
                #[cfg(feature = "cozo")]
                if let Some(materialization) =
                    source.downcast_ref::<FreshCurrentMaterializationErrorV1>()
                {
                    report["publication_state"] =
                        json!(format!("{:?}", materialization.target_publication_state()));
                    report["materialization_phase"] =
                        json!(format!("{:?}", materialization.failure_phase()));
                    report["retained_authority"] = json!(materialization.retains_authority());
                } else {
                    report["publication_state"] = json!(if progress.target_published {
                        "Published"
                    } else {
                        "NotPublished"
                    });
                }
                #[cfg(not(feature = "cozo"))]
                {
                    report["publication_state"] = json!(if progress.target_published {
                        "Published"
                    } else {
                        "NotPublished"
                    });
                }
                if json_output {
                    crate::print_json(&report);
                }
                Err(Box::new(UpgradeFailure {
                    report,
                    source,
                    _source_lease: progress.source_lease,
                    _target_lease: progress.target_lease,
                }))
            }
        }
    }

    pub(super) fn tag_failure_report(error: &AnyErr) -> Option<&Value> {
        error
            .downcast_ref::<UpgradeFailure>()
            .map(|failure| &failure.report)
    }

    pub(super) async fn tag_format_json(
        source: &Path,
        output: Option<&Path>,
        expected_db_id: Ulid,
        options: &mneme_cozo::tag_format_migration::TagFormatMigrationOptions,
        expected_plan: Option<&str>,
    ) -> Result<JsonTagFormatReceipt, AnyErr> {
        use mneme_cozo::tag_format_migration::{
            normalize_tag_format_export, plan_tag_format_export,
        };
        if !source.is_absolute() {
            return Err("JSON migration source must be absolute".into());
        }
        let source = mneme_store_path::resolve_configured_store_path(source)?;
        let lease = Arc::new(StoreLease::acquire(&source)?);
        let source_digest = file_digest(&source)?;
        let mut export = MemStore::load_touchstones_v5(&source)?;
        if export.db_id != expected_db_id {
            return Err("JSON migration database identity mismatch".into());
        }
        let plan = plan_tag_format_export(&export, options)?;
        require_unchanged_source(&source, source_digest, &lease)?;
        if expected_plan.is_none() {
            if output.is_some() {
                return Err("JSON plan does not publish an output".into());
            }
            return Ok(JsonTagFormatReceipt {
                value: json!({"schema":"mneme.tag-format-migration.v1", "action":"plan", "source":source,
                "db_id":expected_db_id.to_string(), "source_sha256":digest_hex(source_digest), "plan":plan,
                "applied":false, "activation":"distinct absent successor; source retained; explicit quiesce/rebind required"}),
                _source_lease: lease,
                _target_lease: None,
            });
        }
        let output = output.ok_or("JSON apply requires a distinct absent --output path")?;
        let (source, target) = preflight_paths(&source, output)?;
        // An absent name can still have an already-admitted owner. File absence
        // is not writer exclusion; retain the target lease through acknowledgement.
        let target_lease = Arc::new(StoreLease::acquire(&target)?);
        let (source, target) = preflight_paths(&source, &target)?;
        target_lease.require_guards(&target)?;
        normalize_tag_format_export(&mut export, options, expected_db_id, expected_plan.unwrap())?;
        // Exact source requests, learned state and volatile records remain in
        // the raw export. MemStore is only a validation/body-inventory view.
        let mut progress = Progress {
            source,
            target,
            operation_id: Ulid::new(),
            phase: "source_export",
            db_id: Some(expected_db_id),
            source_lease: Some(lease.clone()),
            target_lease: Some(target_lease),
            bodies_created: false,
            target_published: false,
            backend: PredecessorBackend::Json,
            target_generation: TargetGeneration::TouchstonesV1,
            tag_export: Some(export.clone()),
            tag_plan: Some(serde_json::to_value(&plan)?),
        };
        let result = finish_export(
            &mut progress,
            PredecessorBackend::Json,
            export,
            Some(source_digest),
            &lease,
        )
        .await;
        match result {
            Ok(mut report) => {
                report["schema"] = json!("mneme.tag-format-migration.v1");
                report["action"] = json!("apply");
                report["applied"] = json!(true);
                report["plan"] = json!(plan);
                report["generation"] = json!("canonical-tags-v1");
                report["activation"] = json!(
                    "not activated; source retained; quiesce old snapshot writers and deliberately rebind owner to successor; never replace source path"
                );
                Ok(JsonTagFormatReceipt {
                    value: report,
                    _source_lease: lease,
                    _target_lease: progress.target_lease,
                })
            }
            Err(error) => {
                let mut report = progress.report("failed");
                report["schema"] = json!("mneme.tag-format-migration.v1");
                report["publication_state"] = json!(if progress.target_published {
                    "Published"
                } else {
                    "Unpublished"
                });
                report["target_generation"] = json!("canonical-tags-v1");
                report["activation"] = json!(
                    "failed result must not be activated; preserve residue and use a new absent output after inspection"
                );
                Err(Box::new(UpgradeFailure {
                    report,
                    source: error,
                    _source_lease: progress.source_lease,
                    _target_lease: progress.target_lease,
                }))
            }
        }
    }

    async fn upgrade(
        progress: &mut Progress,
        backend: PredecessorBackend,
    ) -> Result<Value, AnyErr> {
        #[cfg(not(feature = "cozo"))]
        if matches!(backend, PredecessorBackend::Sqlite) {
            return Err("single-graph-upgrade --backend sqlite requires a cozo-enabled build; no output was created".into());
        }
        let (source, target) = preflight_paths(&progress.source, &progress.target)?;
        progress.source = source;
        progress.target = target;
        progress.phase = "source_admission";
        let lease = Arc::new(StoreLease::acquire(&progress.source)?);
        progress.source_lease = Some(lease.clone());
        if matches!(backend, PredecessorBackend::Json) {
            // An absent JSON name can still belong to an admitted snapshot
            // writer. SQLite materialization owns its own destination lease.
            let target_lease = Arc::new(StoreLease::acquire(&progress.target)?);
            let (source, target) = preflight_paths(&progress.source, &progress.target)?;
            target_lease.require_guards(&target)?;
            progress.source = source;
            progress.target = target;
            progress.target_lease = Some(target_lease);
        }
        if matches!(backend, PredecessorBackend::Sqlite) {
            reject_sidecars(&progress.source)?;
        }
        progress.phase = "source_export";
        let source_digest = if matches!(backend, PredecessorBackend::Json) {
            Some(file_digest(&progress.source)?)
        } else {
            None
        };
        let expected = match backend {
            #[cfg(feature = "cozo")]
            PredecessorBackend::Sqlite => match progress.target_generation {
                TargetGeneration::SingleGraphV1 => {
                    CozoStore::export_single_graph_predecessor(&progress.source, &lease).await?
                }
                TargetGeneration::ConcernV1 => {
                    CozoStore::export_concern_predecessor(&progress.source, &lease).await?
                }
                TargetGeneration::EpisodeContextV2 => {
                    CozoStore::export_episode_context_predecessor(&progress.source, &lease).await?
                }
                TargetGeneration::TouchstonesV1 => {
                    CozoStore::export_touchstones_predecessor(&progress.source, &lease).await?
                }
            },
            #[cfg(not(feature = "cozo"))]
            PredecessorBackend::Sqlite => unreachable!("feature admission above"),
            PredecessorBackend::Json => match progress.target_generation {
                TargetGeneration::SingleGraphV1 => {
                    MemStore::load_legacy_v1(&progress.source).map_err(|error| format!("single-graph-v1 target requires named flat v1 JSON: {error}; v2 JSON requires --target-generation concern-v1"))?.export()
                }
                TargetGeneration::ConcernV1 => {
                    MemStore::load_single_graph_v2(&progress.source).map_err(|error| format!("concern-v1 target requires named single-graph v2 JSON: {error}; flat v1 needs the default single-graph-v1 upgrade first; v3 JSON requires --target-generation episode-context-v2"))?.export()
                }
                TargetGeneration::EpisodeContextV2 => {
                    MemStore::load_concern_v3(&progress.source).map_err(|error| format!("episode-context-v2 target requires named concern v3 JSON: {error}; earlier JSON needs the historical targets first"))?.export()
                }
                TargetGeneration::TouchstonesV1 => {
                    MemStore::load_episode_context_v4(&progress.source).map_err(|error| format!("touchstones-v1 target requires named episode-context v4 JSON: {error}; earlier JSON needs the historical targets first"))?.export()
                }
            },
        };
        if let Some(digest) = source_digest {
            require_unchanged_source(&progress.source, digest, &lease)?;
        }
        finish_export(progress, backend, expected, source_digest, &lease).await
    }

    async fn finish_export(
        progress: &mut Progress,
        backend: PredecessorBackend,
        expected: mneme_cozo::StoreExport,
        source_digest: Option<[u8; 32]>,
        lease: &Arc<StoreLease>,
    ) -> Result<Value, AnyErr> {
        progress.db_id = Some(expected.db_id);
        let non_fs_body_refs = expected
            .nodes
            .iter()
            .filter(|node| !node.body().as_str().starts_with("fs://"))
            .count();
        let body_names =
            portable_body_names(expected.nodes.iter().map(|node| node.body().as_str()))?;
        let node_count = expected.nodes.len();
        let edge_count = expected.edges.len();
        let vector_count = expected.vectors.len();
        let feedback_proofs_reset = if progress.tag_export.is_some() {
            0
        } else {
            expected.feedback_retries.len()
        };
        let snapshot = MemStore::from_export(expected)?;
        lease.require_guards(&progress.source)?;

        if let Some(target_lease) = &progress.target_lease {
            target_lease.require_guards(&progress.target)?;
        }
        progress.phase = "body_preflight";
        let body_root = progress.source.with_extension("bodies");
        if !body_names.is_empty() {
            // Reuse the runtime's existing-only root admission; single portable
            // names below need no alternate BodyRef resolver or serializer.
            let _body_store = FsStore::open_existing(&body_root)?;
        }
        let mut total_bytes = 0u64;
        for name in &body_names {
            let input = read_regular(&body_root.join(name))?;
            total_bytes = total_bytes
                .checked_add(input.metadata()?.len())
                .ok_or("body inventory size overflow")?;
            if total_bytes > MAX_PAYLOAD_BYTES {
                return Err("single-graph-upgrade bodies exceed 512 MiB".into());
            }
        }
        progress.phase = "body_copy";
        let target_root = progress.target.with_extension("bodies");
        #[cfg(test)]
        body_checkpoint("before_body_directory_create")?;
        create_private_dir(&target_root)?;
        progress.bodies_created = true;
        #[cfg(test)]
        body_checkpoint("after_body_directory_create")?;
        let mut records = Vec::with_capacity(body_names.len());
        for name in &body_names {
            records.push(copy_body(
                &body_root.join(name),
                &target_root.join(name),
                name,
            )?);
        }
        let mut manifest = json!({
            "schema": "mneme.single-graph-upgrade.bodies.v1",
            "target_generation": progress.target_generation.as_str(),
            "source": progress.source, "output": progress.target,
            "operation_id": progress.operation_id.to_string(),
            "db_id": progress.db_id.map(|id| id.to_string()),
            "files": records,
        });
        if let Some(plan) = &progress.tag_plan {
            manifest["schema"] = json!("mneme.tag-format-migration.bodies.v1");
            manifest["target_generation"] = json!("canonical-tags-v1");
            manifest["source_sha256"] = json!(source_digest.map(digest_hex));
            manifest["tag_plan"] = plan.clone();
            manifest["historical_source_proofs"] = json!("preserved_exactly");
            manifest["activation"] =
                json!("not activated; source retained; quiesce and rebind to distinct successor");
        }
        #[cfg(test)]
        body_checkpoint("before_manifest_create")?;
        let mut manifest_file = create_private_file(&target_root.join(MANIFEST))?;
        #[cfg(test)]
        body_checkpoint("after_manifest_create")?;
        #[cfg(test)]
        body_checkpoint("before_manifest_write")?;
        serde_json::to_writer_pretty(&mut manifest_file, &manifest)?;
        #[cfg(test)]
        body_checkpoint("after_manifest_write")?;
        #[cfg(test)]
        body_checkpoint("before_manifest_newline")?;
        manifest_file.write_all(b"\n")?;
        #[cfg(test)]
        body_checkpoint("after_manifest_newline")?;
        #[cfg(test)]
        body_checkpoint("before_manifest_sync")?;
        manifest_file.sync_all()?;
        #[cfg(test)]
        body_checkpoint("after_manifest_sync")?;
        #[cfg(test)]
        body_checkpoint("before_body_directory_sync")?;
        File::open(&target_root)?.sync_all()?;
        #[cfg(test)]
        body_checkpoint("after_body_directory_sync")?;
        #[cfg(test)]
        body_checkpoint("before_parent_directory_sync")?;
        File::open(target_root.parent().ok_or("body root has no parent")?)?.sync_all()?;
        #[cfg(test)]
        body_checkpoint("after_parent_directory_sync")?;
        #[cfg(test)]
        body_checkpoint("before_source_guard")?;
        if let Some(digest) = source_digest {
            require_unchanged_source(&progress.source, digest, &lease)?;
        } else {
            lease.require_guards(&progress.source)?;
        }
        #[cfg(test)]
        body_checkpoint("after_source_guard")?;

        // Bodies are complete and durable before the database can be visible.
        // The materializer owns no-clobber staging, full canonical verification,
        // generation fencing, publication and post-publication reopen/verification.
        progress.phase = "database_materialization";
        #[cfg(test)]
        body_checkpoint("before_database_materialization")?;
        let destination_digest = match backend {
            #[cfg(feature = "cozo")]
            PredecessorBackend::Sqlite => {
                match progress.target_generation {
                    TargetGeneration::SingleGraphV1 => {
                        CozoStore::materialize_single_graph(
                            &progress.target,
                            progress.operation_id,
                            &snapshot,
                        )
                        .await?
                    }
                    TargetGeneration::ConcernV1 => {
                        CozoStore::materialize_concern(
                            &progress.target,
                            progress.operation_id,
                            &snapshot,
                        )
                        .await?
                    }
                    TargetGeneration::EpisodeContextV2 => {
                        CozoStore::materialize_episode_context(
                            &progress.target,
                            progress.operation_id,
                            &snapshot,
                        )
                        .await?
                    }
                    TargetGeneration::TouchstonesV1 => {
                        CozoStore::materialize_legacy_touchstones(
                            &progress.target,
                            progress.operation_id,
                            &snapshot,
                        )
                        .await?
                    }
                };
                None
            }
            #[cfg(not(feature = "cozo"))]
            PredecessorBackend::Sqlite => unreachable!("feature admission above"),
            PredecessorBackend::Json => Some(publish_json(
                progress,
                &snapshot,
                source_digest.expect("JSON source digest"),
                &lease,
            )?),
        };
        if let Some(target_lease) = &progress.target_lease {
            target_lease.require_guards(&progress.target)?;
        }
        progress.target_published = true;
        progress.phase = "complete";
        let mut report = progress.report("upgraded_copy");
        report["publication_state"] = json!("Published");
        report["generation"] = json!(progress.target_generation.as_str());
        report["nodes"] = json!(node_count);
        report["edges"] = json!(edge_count);
        report["vectors"] = json!(vector_count);
        report["body_files"] = json!(records.len());
        report["body_bytes"] = json!(total_bytes);
        report["non_fs_body_refs_retained_unfetched"] = json!(non_fs_body_refs);
        report["body_manifest"] = json!(target_root.join(MANIFEST));
        report["volatile_feedback_proofs_reset"] = json!(feedback_proofs_reset);
        if let Some(digest) = source_digest {
            report["source_sha256"] = json!(digest_hex(digest));
        }
        if let Some(digest) = destination_digest {
            report["destination_sha256"] = json!(digest);
        }
        Ok(report)
    }

    fn file_digest(path: &Path) -> Result<[u8; 32], AnyErr> {
        let mut input = read_regular(path)?;
        digest_open_file(&mut input)
    }

    fn digest_open_file(input: &mut File) -> Result<[u8; 32], AnyErr> {
        let mut hash = Sha256::new();
        let mut size = 0_u64;
        let mut buffer = [0_u8; 64 * 1024];
        loop {
            let n = input.read(&mut buffer)?;
            if n == 0 {
                break;
            }
            size = size.checked_add(n as u64).ok_or("source size overflow")?;
            if size > MAX_PAYLOAD_BYTES {
                return Err("single-graph-upgrade source exceeds 512 MiB".into());
            }
            hash.update(&buffer[..n]);
        }
        if input.metadata()?.len() != size {
            return Err("source changed during digest".into());
        }
        Ok(hash.finalize().into())
    }

    fn published_file_digest(path: &Path) -> Result<[u8; 32], AnyErr> {
        // Our staged inode is deliberately linked twice until cleanup, unlike
        // untrusted source/body files which must have exactly one link.
        let mut options = OpenOptions::new();
        options.read(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK);
        }
        let mut input = options.open(path)?;
        if !input.metadata()?.is_file() {
            return Err("published JSON target is not a regular file".into());
        }
        digest_open_file(&mut input)
    }

    fn digest_hex(digest: [u8; 32]) -> String {
        digest.iter().map(|byte| format!("{byte:02x}")).collect()
    }

    fn require_unchanged_source(
        path: &Path,
        expected: [u8; 32],
        lease: &StoreLease,
    ) -> Result<(), AnyErr> {
        lease.require_guards(path)?;
        if file_digest(path)? != expected {
            return Err("single-graph-upgrade source changed during conversion".into());
        }
        lease.require_guards(path)?;
        Ok(())
    }

    fn publish_json(
        progress: &mut Progress,
        snapshot: &MemStore,
        source_digest: [u8; 32],
        lease: &StoreLease,
    ) -> Result<String, AnyErr> {
        // MemStore::save deliberately replaces its target; keep that behavior
        // inside a private stage, then publish the inode under the absent
        // destination name with an atomic, no-replace hard link.
        let parent = progress.target.parent().ok_or("output has no parent")?;
        let stage_dir = parent.join(format!(".single-graph-upgrade-{}", progress.operation_id));
        create_private_dir(&stage_dir)?;
        let staged = stage_dir.join("database.json");
        if let Some(export) = &progress.tag_export {
            let bytes =
                serde_json::to_vec_pretty(&mneme_cozo::StoreExportEnvelopeV6::new(export.clone()))?;
            #[cfg(test)]
            body_checkpoint("before_tag_json_stage_create")?;
            let mut file = OpenOptions::new()
                .write(true)
                .create_new(true)
                .open(&staged)?;
            #[cfg(test)]
            body_checkpoint("after_tag_json_stage_create")?;
            #[cfg(test)]
            body_checkpoint("before_tag_json_stage_write")?;
            file.write_all(&bytes)?;
            #[cfg(test)]
            body_checkpoint("after_tag_json_stage_write")?;
            #[cfg(test)]
            body_checkpoint("before_tag_json_stage_sync")?;
            file.sync_all()?;
            #[cfg(test)]
            body_checkpoint("after_tag_json_stage_sync")?;
        } else {
            match progress.target_generation {
                TargetGeneration::SingleGraphV1 => snapshot.save_single_graph_v2(&staged)?,
                TargetGeneration::ConcernV1 => snapshot.save_concern_v3(&staged)?,
                TargetGeneration::EpisodeContextV2 => snapshot.save_episode_context_v4(&staged)?,
                TargetGeneration::TouchstonesV1 => snapshot.save_touchstones_v5(&staged)?,
            }
        }
        let expected = match &progress.tag_export {
            Some(export) => export.clone().canonical_value()?,
            None => snapshot.export().canonical_value()?,
        };
        if json_target_value(&staged, progress)? != expected {
            return Err("staged JSON graph failed full export readback".into());
        }
        let staged_digest = file_digest(&staged)?;
        File::open(&stage_dir)?.sync_all()?;
        require_unchanged_source(&progress.source, source_digest, lease)?;
        require_absent(&progress.target)?;
        if let Some(target_lease) = &progress.target_lease {
            target_lease.require_guards(&progress.target)?;
        }
        fs::hard_link(&staged, &progress.target)?;
        progress.target_published = true;
        #[cfg(test)]
        body_checkpoint("after_json_publish")?;
        File::open(parent)?.sync_all()?;
        if json_target_value(&progress.target, progress)? != expected
            || published_file_digest(&progress.target)? != staged_digest
        {
            return Err("published JSON graph failed full export/digest readback".into());
        }
        fs::remove_file(&staged)?;
        fs::remove_dir(&stage_dir)?;
        File::open(parent)?.sync_all()?;
        Ok(digest_hex(staged_digest))
    }

    fn json_target_value(path: &Path, progress: &Progress) -> Result<Value, AnyErr> {
        if progress.tag_export.is_some() {
            let bytes = fs::read(path)?;
            let envelope: mneme_cozo::StoreExportEnvelopeV6 = serde_json::from_slice(&bytes)?;
            Ok(envelope.into_store()?.canonical_value()?)
        } else {
            Ok(load_target_json(path, progress.target_generation)?
                .export()
                .canonical_value()?)
        }
    }

    fn load_target_json(path: &Path, generation: TargetGeneration) -> Result<MemStore, AnyErr> {
        Ok(match generation {
            TargetGeneration::SingleGraphV1 => MemStore::load_single_graph_v2(path)?,
            TargetGeneration::ConcernV1 => MemStore::load_concern_v3(path)?,
            TargetGeneration::EpisodeContextV2 => MemStore::load_episode_context_v4(path)?,
            TargetGeneration::TouchstonesV1 => {
                MemStore::from_export(MemStore::load_touchstones_v5(path)?)?
            }
        })
    }

    fn preflight_paths(source: &Path, target: &Path) -> Result<(PathBuf, PathBuf), AnyErr> {
        if !target.is_absolute() {
            return Err("single-graph-upgrade --output must be absolute".into());
        }
        // Keep the final component visible to no-clobber checks, including a
        // dangling symlink. Resolve the existing parent only.
        let parent = target
            .parent()
            .ok_or("output needs a parent")?
            .canonicalize()?;
        let target = parent.join(target.file_name().ok_or("output needs a file name")?);
        if target.with_extension("bodies") == target {
            return Err("output database path cannot also be its own .bodies directory".into());
        }
        require_absent(&target)?;
        require_absent(&target.with_extension("bodies"))?;
        reject_sidecars(&target)?;
        if mneme_store_path::resolve_configured_store_path(&target)? != target {
            return Err("output resolves to an existing or activated store; select an absent conventional path".into());
        }
        let source = mneme_store_path::resolve_configured_store_path(source)?;
        let source = source.canonicalize()?;
        if source
            .components()
            .chain(target.components())
            .any(|part| part.as_os_str() == "generations")
        {
            return Err("single-graph-upgrade does not accept managed generations; use a conventional named predecessor and detached output".into());
        }
        let metadata = fs::symlink_metadata(&source)?;
        if !metadata.is_file() || metadata.len() > MAX_PAYLOAD_BYTES {
            return Err(
                "single-graph-upgrade source must be a regular database of at most 512 MiB".into(),
            );
        }
        if source == target || source.with_extension("bodies") == target.with_extension("bodies") {
            return Err("source and output must have distinct database and body paths".into());
        }
        Ok((source, target))
    }

    fn require_absent(path: &Path) -> Result<(), AnyErr> {
        match fs::symlink_metadata(path) {
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(error) => Err(error.into()),
            Ok(_) => Err(format!(
                "single-graph-upgrade refuses existing path {}",
                path.display()
            )
            .into()),
        }
    }
    fn reject_sidecars(path: &Path) -> Result<(), AnyErr> {
        for suffix in ["-wal", "-shm", "-journal"] {
            let mut name = path.as_os_str().to_owned();
            name.push(suffix);
            require_absent(Path::new(&name))?;
        }
        Ok(())
    }
    fn portable_body_names<'a>(
        refs: impl IntoIterator<Item = &'a str>,
    ) -> Result<BTreeSet<String>, AnyErr> {
        let mut names = BTreeSet::new();
        for body in refs {
            // Copy and verify only local fs:// assets. All other valid body
            // references are preserved by exact export/readback, without
            // fetching or claiming to verify external content.
            let Some(name) = body.strip_prefix("fs://") else {
                continue;
            };
            let mut parts = Path::new(name).components();
            if !matches!(parts.next(), Some(Component::Normal(_)))
                || parts.next().is_some()
                || name.contains('/')
                || name == MANIFEST
            {
                return Err("single-graph-upgrade requires single relative fs:// filenames; absolute or nonportable legacy body references need a separate explicit conversion".into());
            }
            names.insert(name.to_owned());
        }
        Ok(names)
    }
    fn read_regular(path: &Path) -> Result<File, AnyErr> {
        let mut options = OpenOptions::new();
        options.read(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK);
        }
        let file = options.open(path)?;
        let metadata = file.metadata()?;
        if !metadata.is_file() {
            return Err("body source is not a regular file".into());
        }
        #[cfg(unix)]
        {
            use std::os::unix::fs::MetadataExt;
            if metadata.nlink() != 1 {
                return Err("body source has multiple hard links".into());
            }
        }
        if metadata.len() > MAX_PAYLOAD_BYTES {
            return Err("body source exceeds 512 MiB".into());
        }
        Ok(file)
    }
    fn create_private_dir(path: &Path) -> Result<(), AnyErr> {
        let mut builder = fs::DirBuilder::new();
        #[cfg(unix)]
        {
            use std::os::unix::fs::DirBuilderExt;
            builder.mode(0o700);
        }
        builder.create(path)?;
        Ok(())
    }
    fn create_private_file(path: &Path) -> Result<File, AnyErr> {
        let mut options = OpenOptions::new();
        options.write(true).create_new(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.mode(0o600);
        }
        Ok(options.open(path)?)
    }
    #[derive(Serialize)]
    struct BodyRecord {
        name: String,
        size: u64,
        sha256: String,
    }
    fn copy_body(source: &Path, target: &Path, name: &str) -> Result<BodyRecord, AnyErr> {
        let mut input = read_regular(source)?;
        let expected_size = input.metadata()?.len();
        #[cfg(test)]
        body_checkpoint("before_body_file_create")?;
        let mut output = create_private_file(target)?;
        #[cfg(test)]
        body_checkpoint("after_body_file_create")?;
        let mut hash = Sha256::new();
        let mut total = 0u64;
        let mut buffer = [0u8; 64 * 1024];
        loop {
            #[cfg(test)]
            body_checkpoint("before_body_read")?;
            let n = input.read(&mut buffer)?;
            #[cfg(test)]
            body_checkpoint("after_body_read")?;
            if n == 0 {
                break;
            }
            total += n as u64;
            if total > expected_size {
                return Err("body source grew during copy".into());
            }
            #[cfg(test)]
            body_checkpoint("before_body_write")?;
            output.write_all(&buffer[..n])?;
            #[cfg(test)]
            body_checkpoint("after_body_write")?;
            hash.update(&buffer[..n]);
        }
        #[cfg(test)]
        body_checkpoint("before_body_file_sync")?;
        output.sync_all()?;
        #[cfg(test)]
        body_checkpoint("after_body_file_sync")?;
        if total != expected_size || input.metadata()?.len() != expected_size {
            return Err("body source changed during copy".into());
        }
        let digest = format!("{:x}", hash.finalize());
        #[cfg(test)]
        body_checkpoint("before_body_readback")?;
        let mut check = read_regular(target)?;
        let mut hash = Sha256::new();
        let mut verified = 0u64;
        loop {
            let n = check.read(&mut buffer)?;
            if n == 0 {
                break;
            }
            verified += n as u64;
            if verified > expected_size {
                return Err("body copy grew during verification".into());
            }
            hash.update(&buffer[..n]);
        }
        if verified != expected_size || format!("{:x}", hash.finalize()) != digest {
            return Err("body copy failed size/digest verification".into());
        }
        #[cfg(test)]
        body_checkpoint("after_body_readback")?;
        Ok(BodyRecord {
            name: name.to_owned(),
            size: total,
            sha256: digest,
        })
    }

    #[cfg(test)]
    std::thread_local! {
        // No environment or operational knob: only this unit-test binary can
        // arm a failure. Production calls and state are compiled out entirely.
        static BODY_FAILURE: std::cell::Cell<Option<&'static str>> = const { std::cell::Cell::new(None) };
    }

    #[cfg(test)]
    fn body_checkpoint(at: &'static str) -> Result<(), AnyErr> {
        if BODY_FAILURE.with(|slot| {
            if slot.get() == Some(at) {
                slot.set(None);
                true
            } else {
                false
            }
        }) {
            return Err(std::io::Error::other(format!(
                "injected single-graph-upgrade body failure at {at}"
            ))
            .into());
        }
        Ok(())
    }

    #[cfg(test)]
    mod tests {
        use super::*;

        #[tokio::test]
        async fn historical_touchstones_json_target_keeps_legacy_tags_exact() {
            use mneme_core::{BodyRef, Node, NodeId, NodeStatus, Provenance};
            let root = std::env::temp_dir()
                .canonicalize()
                .unwrap()
                .join(format!("mneme-json-v4-v5-{}", Ulid::new()));
            fs::create_dir(&root).unwrap();
            let source = root.join("v4.json");
            let target = root.join("v5.json");
            let mut export = MemStore::new(4).export();
            export.nodes.push(
                Node::try_new(
                    NodeId(Ulid::new()),
                    "legacy tag",
                    BodyRef::new("inline://legacy").unwrap(),
                    ["Old_Tag"],
                    Provenance::derived_empty(),
                    0.5,
                    0.6,
                    NodeStatus::Active,
                    1,
                )
                .unwrap(),
            );
            let before =
                serde_json::to_vec(&mneme_cozo::StoreExportEnvelopeV4::new(export)).unwrap();
            fs::write(&source, &before).unwrap();
            let mut progress = Progress {
                source: source.clone(),
                target: target.clone(),
                operation_id: Ulid::new(),
                phase: "preflight",
                db_id: None,
                source_lease: None,
                target_lease: None,
                bodies_created: false,
                target_published: false,
                backend: PredecessorBackend::Json,
                target_generation: TargetGeneration::TouchstonesV1,
                tag_export: None,
                tag_plan: None,
            };
            let held_target = StoreLease::acquire(&target).unwrap();
            assert!(
                upgrade(&mut progress, PredecessorBackend::Json)
                    .await
                    .is_err()
            );
            assert!(!target.exists());
            assert!(!target.with_extension("bodies").exists());
            assert_eq!(fs::read(&source).unwrap(), before);
            drop(held_target);
            progress.source_lease = None;
            upgrade(&mut progress, PredecessorBackend::Json)
                .await
                .unwrap();
            let decoded = MemStore::load_touchstones_v5(&target).unwrap();
            assert_eq!(decoded.nodes[0].tags().collect::<Vec<_>>(), vec!["Old_Tag"]);
            assert!(MemStore::load(&target).is_err());
            assert_eq!(fs::read(&source).unwrap(), before);
            drop(progress);
            fs::remove_dir_all(root).unwrap();
        }

        #[tokio::test]
        async fn json_tag_migration_preserves_bodies_exact_source_and_no_clobber() {
            use mneme_core::{BodyRef, Node, NodeId, NodeStatus, Provenance};
            use mneme_cozo::tag_format_migration::TagFormatMigrationOptions;
            let root = std::env::temp_dir()
                .canonicalize()
                .unwrap()
                .join(format!("mneme-json-tag-{}", Ulid::new()));
            fs::create_dir(&root).unwrap();
            let source = root.join("source.json");
            let output = root.join("target.json");
            let mut export = MemStore::new(4).export();
            let node = Node::try_new(
                NodeId(Ulid::new()),
                "legacy tag",
                BodyRef::new("fs://body").unwrap(),
                ["Old_Tag"],
                Provenance::derived_empty(),
                0.5,
                0.6,
                NodeStatus::Active,
                1,
            )
            .unwrap();
            export.nodes.push(node);
            export
                .feedback_retries
                .push(mneme_cozo::FeedbackRetryRecord {
                    key: "historical-diagnostic-token".into(),
                    fingerprint: "a".repeat(64),
                    epoch: Ulid::new().to_string(),
                    sequence: 7,
                });
            let exact_retries = export.feedback_retries.clone();

            let bytes = serde_json::to_vec(&mneme_cozo::StoreExportEnvelopeV5::new(export.clone()))
                .unwrap();
            fs::write(&source, &bytes).unwrap();
            create_private_dir(&source.with_extension("bodies")).unwrap();
            fs::write(source.with_extension("bodies").join("body"), b"exact body").unwrap();
            let options = TagFormatMigrationOptions::default();
            let plan = tag_format_json(&source, None, export.db_id, &options, None)
                .await
                .unwrap();
            assert!(!output.exists());
            assert!(!output.with_extension("bodies").exists());
            let digest_owned = plan["plan"]["plan_sha256"].as_str().unwrap().to_owned();
            drop(plan);
            let digest = digest_owned.as_str();
            assert!(
                tag_format_json(&source, Some(&output), Ulid::new(), &options, Some(digest))
                    .await
                    .is_err()
            );
            assert!(
                tag_format_json(
                    &source,
                    Some(&output),
                    export.db_id,
                    &options,
                    Some(&"0".repeat(64))
                )
                .await
                .is_err()
            );
            assert!(!output.with_extension("bodies").exists());
            let held_target = root.join("already-admitted.json");
            let held_lease = StoreLease::acquire(&held_target).unwrap();
            assert!(
                tag_format_json(
                    &source,
                    Some(&held_target),
                    export.db_id,
                    &options,
                    Some(digest)
                )
                .await
                .is_err()
            );
            assert!(!held_target.exists());
            assert!(!held_target.with_extension("bodies").exists());
            drop(held_lease);
            for (index, point) in [
                "before_tag_json_stage_create",
                "after_tag_json_stage_create",
                "before_tag_json_stage_write",
                "after_tag_json_stage_write",
                "before_tag_json_stage_sync",
                "after_tag_json_stage_sync",
            ]
            .into_iter()
            .enumerate()
            {
                let cut = root.join(format!("stage-cut-{index}.json"));
                BODY_FAILURE.with(|slot| slot.set(Some(point)));
                let error =
                    tag_format_json(&source, Some(&cut), export.db_id, &options, Some(digest))
                        .await
                        .unwrap_err();
                BODY_FAILURE.with(|slot| slot.set(None));
                let failure = error.downcast_ref::<UpgradeFailure>().unwrap();
                assert_eq!(failure.report["publication_state"], "Unpublished");
                assert!(!cut.exists());
                assert_eq!(fs::read(&source).unwrap(), bytes);
                assert!(cut.with_extension("bodies").join(MANIFEST).exists());
                drop(error);
            }
            let report =
                tag_format_json(&source, Some(&output), export.db_id, &options, Some(digest))
                    .await
                    .unwrap();
            assert_eq!(report["volatile_feedback_proofs_reset"], 0);
            assert_eq!(fs::read(&source).unwrap(), bytes);
            assert_eq!(
                fs::read(output.with_extension("bodies").join("body")).unwrap(),
                b"exact body"
            );
            let target: mneme_cozo::StoreExportEnvelopeV6 =
                serde_json::from_slice(&fs::read(&output).unwrap()).unwrap();
            let target = target.into_store().unwrap();
            assert_eq!(target.feedback_retries, exact_retries);

            assert_eq!(target.nodes[0].tags().collect::<Vec<_>>(), vec!["old-tag"]);
            let manifest: Value = serde_json::from_slice(
                &fs::read(output.with_extension("bodies").join(MANIFEST)).unwrap(),
            )
            .unwrap();
            assert_eq!(manifest["tag_plan"]["plan_sha256"], digest);
            assert!(StoreLease::acquire(&output).is_err());
            assert!(StoreLease::acquire(&source).is_err());
            drop(report);
            drop(StoreLease::acquire(&output).unwrap());
            let published = fs::read(&output).unwrap();
            assert!(
                tag_format_json(&source, Some(&output), export.db_id, &options, Some(digest))
                    .await
                    .is_err()
            );
            assert_eq!(fs::read(&output).unwrap(), published);
            let cut_output = root.join("cut.json");
            BODY_FAILURE.with(|slot| slot.set(Some("after_json_publish")));
            let error = tag_format_json(
                &source,
                Some(&cut_output),
                export.db_id,
                &options,
                Some(digest),
            )
            .await
            .unwrap_err();
            BODY_FAILURE.with(|slot| slot.set(None));
            let failure = error.downcast_ref::<UpgradeFailure>().unwrap();
            assert_eq!(failure.report["publication_state"], "Published");
            assert_eq!(fs::read(&source).unwrap(), bytes);
            assert!(cut_output.exists());
            assert_eq!(
                fs::read(cut_output.with_extension("bodies").join("body")).unwrap(),
                b"exact body"
            );
            drop(error);
            fs::remove_dir_all(root).unwrap();
        }

        async fn populated_predecessor() -> MemStore {
            use mneme_core::ports::{EmbeddingMetadataStore, GraphStore, VectorIndex};
            use mneme_core::{BodyRef, Node, NodeId, NodeStatus, Provenance};
            let store = MemStore::new(4);
            store
                .set_embedding_fingerprint(&mneme_embed::hashing_fingerprint(4))
                .unwrap();
            let node = Node::try_new(
                NodeId(Ulid::new()),
                "named predecessor",
                BodyRef::new("inline://retained-body").unwrap(),
                ["fixture"],
                Provenance::derived_empty(),
                0.5,
                0.6,
                NodeStatus::Active,
                1,
            )
            .unwrap();
            store.put_node(&node).await.unwrap();
            store
                .upsert(node.id(), &[1.0, 0.0, 0.0, 0.0])
                .await
                .unwrap();
            store
        }

        #[tokio::test(flavor = "current_thread")]
        async fn fs_body_copy_checkpoints_report_exact_prepublication_residue() {
            use mneme_core::concern::*;
            use mneme_core::ports::{EmbeddingMetadataStore, GraphStore, VectorIndex};
            use mneme_core::{
                BodyRef, BodySpan, Edge, EdgeKind, Node, NodeId, NodeStatus, Provenance,
            };

            for target_generation in [
                TargetGeneration::SingleGraphV1,
                TargetGeneration::ConcernV1,
                TargetGeneration::EpisodeContextV2,
            ] {
                let root = std::env::temp_dir()
                    .canonicalize()
                    .unwrap()
                    .join(format!("mneme-body-copy-cuts-{}", Ulid::new()));
                fs::create_dir(&root).unwrap();
                let source = root.join("old.json");
                let body_root = source.with_extension("bodies");
                create_private_dir(&body_root).unwrap();
                let original_body = b"a real fs-backed body that must never change";
                let mut body_file = create_private_file(&body_root.join("body")).unwrap();
                body_file.write_all(original_body).unwrap();
                body_file.sync_all().unwrap();
                drop(body_file);
                let old = MemStore::new(4);
                old.set_embedding_fingerprint(&mneme_embed::hashing_fingerprint(4))
                    .unwrap();
                let node = Node::try_new(
                    NodeId(Ulid::new()),
                    "fs-backed predecessor",
                    BodyRef::new("fs://body").unwrap(),
                    ["fixture"],
                    Provenance::derived_empty(),
                    0.5,
                    0.6,
                    NodeStatus::Active,
                    1,
                )
                .unwrap();
                old.put_node(&node).await.unwrap();
                old.upsert(node.id(), &[1.0, 0.0, 0.0, 0.0]).await.unwrap();
                let old = if target_generation == TargetGeneration::EpisodeContextV2 {
                    // Persisted prior-codec bindings are opaque evidence. These
                    // frozen semantic v1 digests are never recomputed/rebound.
                    let claim = |id, summary| {
                        Node::try_new(
                            NodeId(Ulid::from(id)),
                            summary,
                            BodyRef::new("inline://concern").unwrap(),
                            std::iter::empty::<&str>(),
                            Provenance::derived_empty(),
                            0.5,
                            0.5,
                            NodeStatus::Active,
                            1,
                        )
                        .unwrap()
                    };
                    let a = claim(101_u128, "bounded left claim");
                    let b = claim(102_u128, "bounded right claim");
                    old.put_node(&a).await.unwrap();
                    old.put_node(&b).await.unwrap();
                    let edge = Edge::from_stored(
                        a.id(),
                        b.id(),
                        EdgeKind::Associative,
                        Some(BodySpan::new(2, 8)),
                        0.3125,
                        123,
                        43,
                        7,
                    );
                    old.put_edge(&edge).await.unwrap();
                    let binding = ConcernBinding::new(
                        ConcernKind::Disagreement,
                        ConcernEndpoint::new(
                            a.id(),
                            ConcernDigest::from_hex(
                                "b069ca90a1ca30642c5d125afc440d15e6863ea5d33ee27dbc77c6652eb757d3",
                            )
                            .unwrap(),
                        ),
                        ConcernEndpoint::new(
                            b.id(),
                            ConcernDigest::from_hex(
                                "8b9ba55d40a8adcf76afd15959618f3232e35b423be439d8077cebffd44fe106",
                            )
                            .unwrap(),
                        ),
                    )
                    .unwrap();
                    let notice = ConcernNotice::new(
                        binding,
                        "Stored prior inspection of the claims",
                        "Which deployment was inspected?",
                    )
                    .unwrap();
                    let finding = ScopedConcernFinding::new(
                        "historical fixture deployment",
                        "The fs-backed witness retains the inspected bytes",
                        vec![
                            ConcernEvidence::new(
                                "fs://body",
                                ConcernDigest::of_bytes(original_body),
                            )
                            .unwrap(),
                        ],
                    )
                    .unwrap();
                    let mut row = serde_json::to_value(ConcernRow::from_notice(notice)).unwrap();
                    row["finding"] = serde_json::to_value(finding).unwrap();
                    let mut export = old.export();
                    export.concerns = vec![serde_json::from_value(row).unwrap()];
                    MemStore::from_export(export).unwrap()
                } else {
                    old
                };
                if matches!(target_generation, TargetGeneration::SingleGraphV1) {
                    let mut legacy = serde_json::to_value(old.export()).unwrap();
                    legacy.as_object_mut().unwrap().remove("concerns");
                    legacy.as_object_mut().unwrap().remove("touchstones");
                    fs::write(&source, serde_json::to_vec(&legacy).unwrap()).unwrap();
                } else if matches!(target_generation, TargetGeneration::ConcernV1) {
                    // The exact predecessor of historical v3 is v2.
                    old.save_single_graph_v2(&source).unwrap();
                } else {
                    // The exact predecessor of current v4 is frozen v3.
                    old.save_concern_v3(&source).unwrap();
                }
                let original_source = fs::read(&source).unwrap();

                for (index, point) in [
                    "before_body_directory_create",
                    "after_body_directory_create",
                    "before_body_file_create",
                    "after_body_file_create",
                    "before_body_read",
                    "after_body_read",
                    "before_body_write",
                    "after_body_write",
                    "before_body_file_sync",
                    "after_body_file_sync",
                    "before_body_readback",
                    "after_body_readback",
                    "before_manifest_create",
                    "after_manifest_create",
                    "before_manifest_write",
                    "after_manifest_write",
                    "before_manifest_newline",
                    "after_manifest_newline",
                    "before_manifest_sync",
                    "after_manifest_sync",
                    "before_body_directory_sync",
                    "after_body_directory_sync",
                    "before_parent_directory_sync",
                    "after_parent_directory_sync",
                    "before_source_guard",
                    "after_source_guard",
                    "before_database_materialization",
                ]
                .into_iter()
                .enumerate()
                {
                    // Keep each cut independent of immediate flock reacquisition.
                    // Parallel subprocess tests may briefly inherit a lease until
                    // CLOEXEC closes it; isolated execution passes, but that is an
                    // inference about the observed contention, not a proven cause.
                    // Use fresh source/body inodes, never retry an ownership refusal
                    // or serialize the suite. The untouched template serves success.
                    let case_root = root.join(format!("source-{index}"));
                    create_private_dir(&case_root).unwrap();
                    let source = case_root.join("old.json");
                    fs::write(&source, &original_source).unwrap();
                    let body_root = source.with_extension("bodies");
                    create_private_dir(&body_root).unwrap();
                    let mut body_file = create_private_file(&body_root.join("body")).unwrap();
                    body_file.write_all(original_body).unwrap();
                    body_file.sync_all().unwrap();
                    drop(body_file);
                    let target = root.join(format!("output-{index}.json"));
                    let target_bodies = target.with_extension("bodies");
                    BODY_FAILURE.with(|slot| slot.set(Some(point)));
                    let error = run(
                        &source,
                        &target,
                        PredecessorBackend::Json,
                        target_generation,
                        false,
                    )
                    .await
                    .unwrap_err();
                    BODY_FAILURE.with(|slot| slot.set(None));
                    let failure = error.downcast_ref::<UpgradeFailure>().unwrap();
                    assert!(
                        failure.source.to_string().contains(point),
                        "{point}: {failure}"
                    );
                    let report = &failure.report;
                    assert_eq!(
                        report["phase"],
                        if point == "before_database_materialization" {
                            "database_materialization"
                        } else {
                            "body_copy"
                        }
                    );
                    assert_eq!(report["publication_state"], "NotPublished");
                    assert_eq!(report["source_replaced"], false);
                    assert_eq!(report["activated"], false);
                    assert_eq!(report["output"], target.to_str().unwrap());
                    assert_eq!(report["bodies"], target_bodies.to_str().unwrap());
                    assert!(
                        report["recovery"]
                            .as_str()
                            .unwrap()
                            .contains("Preserve and inspect")
                    );
                    assert!(!target.exists(), "{point}: no database may be visible");
                    let bodies_created = point != "before_body_directory_create";
                    assert_eq!(report["bodies_created"], bodies_created);
                    assert_eq!(target_bodies.exists(), bodies_created);
                    assert!(!Path::new(report["stage"].as_str().unwrap()).exists());
                    assert_eq!(fs::read(&source).unwrap(), original_source);
                    assert_eq!(fs::read(body_root.join("body")).unwrap(), original_body);
                    drop(error);
                }
                let target = root.join("success.json");
                let mut success = Progress {
                    source: source.clone(),
                    target: target.clone(),
                    operation_id: Ulid::new(),
                    phase: "preflight",
                    db_id: None,
                    source_lease: None,
                    target_lease: None,
                    bodies_created: false,
                    target_published: false,
                    backend: PredecessorBackend::Json,
                    target_generation,
                    tag_export: None,
                    tag_plan: None,
                };
                let report = upgrade(&mut success, PredecessorBackend::Json)
                    .await
                    .unwrap();
                assert_eq!(report["body_files"], 1);
                assert_eq!(report["body_bytes"], original_body.len());
                assert_eq!(report["publication_state"], "Published");
                if target_generation == TargetGeneration::EpisodeContextV2 {
                    assert_eq!(
                        report["binding_codec_transition"]["content_fingerprint"]["to"],
                        mneme_core::ports::ROUTING_CONTENT_FINGERPRINT_CODEC
                    );
                    assert_eq!(
                        report["binding_codec_transition"]["edge_fingerprint"]["to"],
                        mneme_core::ports::ROUTING_EDGE_FINGERPRINT_CODEC
                    );
                    assert_eq!(
                        report["binding_codec_transition"]["historical_meaning_hashes"],
                        "preserved_without_rebinding"
                    );
                    assert_eq!(
                        report["binding_codec_transition"]["accumulated_edge_learning"],
                        "preserved"
                    );
                } else {
                    assert!(
                        report.get("binding_codec_transition").is_none(),
                        "historical target reports remain unchanged"
                    );
                }
                assert_eq!(
                    fs::read(target.with_extension("bodies").join("body")).unwrap(),
                    original_body
                );
                assert_eq!(fs::read(&source).unwrap(), original_source);
                assert_eq!(fs::read(body_root.join("body")).unwrap(), original_body);
                assert_eq!(
                    match target_generation {
                        TargetGeneration::SingleGraphV1 =>
                            MemStore::load_single_graph_v2(&target).unwrap().db_id(),
                        TargetGeneration::ConcernV1 =>
                            MemStore::load_concern_v3(&target).unwrap().db_id(),
                        TargetGeneration::EpisodeContextV2 =>
                            MemStore::load_episode_context_v4(&target).unwrap().db_id(),
                        TargetGeneration::TouchstonesV1 => MemStore::load(&target).unwrap().db_id(),
                    },
                    old.db_id()
                );
                if target_generation == TargetGeneration::EpisodeContextV2 {
                    let copied = MemStore::load_episode_context_v4(&target).unwrap();
                    assert_eq!(
                        copied.export().canonical_value().unwrap(),
                        old.export().canonical_value().unwrap(),
                        "witness note, learned edge state and historical concern bytes are preserved"
                    );
                    let row = copied.export().concerns[0].clone();
                    assert!(
                        matches!(copied.update_concern(&ConcernUpdate::RecordScopedFinding { expected: row.clone(), finding: row.finding().unwrap().clone() }).await.unwrap(), ConcernCommitOutcome::Refused { reason: ConcernRefusal::StaleMeanings, row: Some(stored) } if stored == row)
                    );
                    assert_eq!(
                        copied.get_concern(&row.binding().key()).await.unwrap(),
                        Some(row)
                    );
                    assert_eq!(
                        fs::read(target.with_extension("bodies").join("body")).unwrap(),
                        original_body
                    );
                }
                drop(success);
                if matches!(target_generation, TargetGeneration::SingleGraphV1) {
                    let concern = root.join("concern-success.json");
                    run(
                        &target,
                        &concern,
                        PredecessorBackend::Json,
                        TargetGeneration::ConcernV1,
                        false,
                    )
                    .await
                    .unwrap();
                    assert_eq!(
                        MemStore::load_concern_v3(&concern).unwrap().db_id(),
                        old.db_id()
                    );
                    assert_eq!(
                        fs::read(concern.with_extension("bodies").join("body")).unwrap(),
                        original_body
                    );
                    assert_eq!(fs::read(&source).unwrap(), original_source);
                }
                fs::remove_dir_all(root).unwrap();
            }
        }

        #[tokio::test]
        async fn concern_json_multi_vector_publication_preserves_canonical_export() {
            use mneme_core::ports::{GraphStore, VectorIndex};
            use mneme_core::{BodyRef, Node, NodeId, NodeStatus, Provenance};
            let root = std::env::temp_dir()
                .canonicalize()
                .unwrap()
                .join(format!("mneme-concern-multi-vector-{}", Ulid::new()));
            fs::create_dir(&root).unwrap();
            let source = root.join("single-graph.json");
            let target = root.join("concern.json");
            let predecessor = populated_predecessor().await;
            for index in 0..31 {
                let node = Node::try_new(
                    NodeId(Ulid::new()),
                    format!("vector node {index}"),
                    BodyRef::new("inline://multi-vector").unwrap(),
                    ["fixture"],
                    Provenance::derived_empty(),
                    0.5,
                    0.6,
                    NodeStatus::Active,
                    1,
                )
                .unwrap();
                predecessor.put_node(&node).await.unwrap();
                let mut vector = [0.0; 4];
                vector[index % 4] = 1.0;
                predecessor.upsert(node.id(), &vector).await.unwrap();
            }
            assert_eq!(predecessor.export().vectors.len(), 32);
            predecessor.save_single_graph_v2(&source).unwrap();
            let before = fs::read(&source).unwrap();
            run(
                &source,
                &target,
                PredecessorBackend::Json,
                TargetGeneration::ConcernV1,
                false,
            )
            .await
            .unwrap();
            assert_eq!(fs::read(&source).unwrap(), before);
            assert_eq!(
                MemStore::load_concern_v3(&target)
                    .unwrap()
                    .export()
                    .canonical_value()
                    .unwrap(),
                predecessor.export().canonical_value().unwrap()
            );
            assert!(MemStore::load_single_graph_v2(&target).is_err());
            assert!(MemStore::load(&source).is_err());
            assert!(fs::read_dir(&root).unwrap().all(|entry| {
                !entry
                    .unwrap()
                    .file_name()
                    .to_string_lossy()
                    .starts_with(".single-graph-upgrade-")
            }));
            fs::remove_dir_all(root).unwrap();
        }

        #[tokio::test]
        async fn concern_json_target_requires_exact_v2_and_retains_source() {
            let root = std::env::temp_dir()
                .canonicalize()
                .unwrap()
                .join(format!("mneme-concern-json-{}", Ulid::new()));
            fs::create_dir(&root).unwrap();
            let source = root.join("single-graph.json");
            let predecessor = populated_predecessor().await;
            predecessor.save_single_graph_v2(&source).unwrap();
            let before = fs::read(&source).unwrap();
            let target = root.join("concern.json");
            let mut progress = Progress {
                source: source.clone(),
                target: target.clone(),
                operation_id: Ulid::new(),
                phase: "preflight",
                db_id: None,
                source_lease: None,
                target_lease: None,
                bodies_created: false,
                target_published: false,
                backend: PredecessorBackend::Json,
                target_generation: TargetGeneration::ConcernV1,
                tag_export: None,
                tag_plan: None,
            };
            let report = upgrade(&mut progress, PredecessorBackend::Json)
                .await
                .unwrap();
            assert_eq!(report["generation"], "concern-v1");
            assert_eq!(report["source_replaced"], false);
            assert_eq!(report["activated"], false);
            assert_eq!(
                MemStore::load_concern_v3(&target).unwrap().db_id(),
                predecessor.db_id()
            );
            assert!(MemStore::load_single_graph_v2(&target).is_err());
            assert!(MemStore::load(&source).is_err());
            assert_eq!(fs::read(&source).unwrap(), before);
            assert_eq!(
                serde_json::to_value(MemStore::load_concern_v3(&target).unwrap().export()).unwrap(),
                serde_json::to_value(predecessor.export()).unwrap()
            );
            drop(progress);

            let ambiguous = root.join("ambiguous.json");
            BODY_FAILURE.with(|slot| slot.set(Some("after_json_publish")));
            let error = run(
                &source,
                &ambiguous,
                PredecessorBackend::Json,
                TargetGeneration::ConcernV1,
                true,
            )
            .await
            .unwrap_err();
            BODY_FAILURE.with(|slot| slot.set(None));
            let failure = error.downcast_ref::<UpgradeFailure>().unwrap();
            assert_eq!(failure.report["publication_state"], "Published");
            assert_eq!(failure.report["source_replaced"], false);
            assert_eq!(
                MemStore::load_concern_v3(&ambiguous).unwrap().db_id(),
                predecessor.db_id()
            );
            assert_eq!(fs::read(&source).unwrap(), before);
            drop(error);

            // Neither flat v1 nor already-current v3 is silently guessed.
            for (name, bytes) in [
                ("flat", {
                    let mut value = serde_json::to_value(predecessor.export()).unwrap();
                    value.as_object_mut().unwrap().remove("concerns");
                    serde_json::to_vec(&value).unwrap()
                }),
                ("current", fs::read(&target).unwrap()),
            ] {
                let source = root.join(format!("{name}.json"));
                fs::write(&source, &bytes).unwrap();
                let output = root.join(format!("{name}-output.json"));
                let error = run(
                    &source,
                    &output,
                    PredecessorBackend::Json,
                    TargetGeneration::ConcernV1,
                    true,
                )
                .await
                .unwrap_err();
                let failure = error.downcast_ref::<UpgradeFailure>().unwrap();
                assert_eq!(failure.report["phase"], "source_export");
                assert_eq!(failure.report["target_generation"], "concern-v1");
                assert!(!output.exists());
                assert!(!output.with_extension("bodies").exists());
                assert_eq!(fs::read(&source).unwrap(), bytes);
                drop(error);
            }
            fs::remove_dir_all(root).unwrap();
        }

        #[cfg(feature = "cozo")]
        #[tokio::test]
        async fn concern_sqlite_target_upgrades_exact_single_graph_without_activation() {
            let root = std::env::temp_dir()
                .canonicalize()
                .unwrap()
                .join(format!("mneme-concern-sqlite-{}", Ulid::new()));
            fs::create_dir(&root).unwrap();
            let source = root.join("single-graph.db");
            let predecessor = populated_predecessor().await;
            CozoStore::materialize_single_graph(&source, Ulid::new(), &predecessor)
                .await
                .unwrap();
            let before = fs::read(&source).unwrap();
            let target = root.join("concern.db");
            let mut progress = Progress {
                source: source.clone(),
                target: target.clone(),
                operation_id: Ulid::new(),
                phase: "preflight",
                db_id: None,
                source_lease: None,
                target_lease: None,
                bodies_created: false,
                target_published: false,
                backend: PredecessorBackend::Sqlite,
                target_generation: TargetGeneration::ConcernV1,
                tag_export: None,
                tag_plan: None,
            };
            let report = upgrade(&mut progress, PredecessorBackend::Sqlite)
                .await
                .unwrap();
            assert_eq!(report["generation"], "concern-v1");
            assert_eq!(report["source_replaced"], false);
            assert_eq!(report["activated"], false);
            let target_lease = StoreLease::acquire(&target).unwrap();
            let target_before = fs::read(&target).unwrap();
            assert!(CozoStore::require_existing_current(&target, &target_lease).is_err());
            let exported = CozoStore::export_episode_context_predecessor(&target, &target_lease)
                .await
                .unwrap();
            let historical = MemStore::from_export(exported).unwrap();
            assert_eq!(
                historical.export().canonical_value().unwrap(),
                predecessor.export().canonical_value().unwrap()
            );
            assert_eq!(fs::read(&target).unwrap(), target_before);
            drop(target_lease);
            assert_eq!(fs::read(&source).unwrap(), before);
            assert!(
                StoreLease::acquire(&source).is_err(),
                "source lease is retained until completion"
            );
            drop(progress);
            fs::remove_dir_all(root).unwrap();
        }

        #[test]
        fn body_inventory_keeps_non_fs_refs_but_refuses_nonportable_fs_names() {
            let names = portable_body_names([
                "inline://body-bound-by-export",
                "https://example.invalid/not-fetched",
                "fs://local-body",
            ])
            .unwrap();
            assert_eq!(names.into_iter().collect::<Vec<_>>(), vec!["local-body"]);
            assert!(portable_body_names(["fs://../escape"]).is_err());
            assert!(portable_body_names(["fs:///absolute"]).is_err());
        }

        #[test]
        fn detached_target_requires_absolute_absent_path_before_source_admission() {
            let root = std::env::temp_dir().join(format!("mneme-single-graph-cli-{}", Ulid::new()));
            fs::create_dir(&root).unwrap();
            let source = root.join("missing.db");
            assert!(preflight_paths(&source, Path::new("relative.db")).is_err());
            let output = root.join("output.db");
            fs::write(&output, b"valuable").unwrap();
            assert!(preflight_paths(&source, &output).is_err());
            assert_eq!(fs::read(&output).unwrap(), b"valuable");
            fs::remove_dir_all(root).unwrap();
        }

        #[test]
        fn json_publication_distinguishes_preflight_refusal_from_visible_ambiguous_failure() {
            let root =
                std::env::temp_dir().join(format!("mneme-single-graph-json-{}", Ulid::new()));
            fs::create_dir(&root).unwrap();
            let source = root.join("old.json");
            let old = MemStore::new(4);
            let mut legacy = serde_json::to_value(old.export()).unwrap();
            legacy.as_object_mut().unwrap().remove("concerns");
            legacy.as_object_mut().unwrap().remove("touchstones");
            fs::write(&source, serde_json::to_vec(&legacy).unwrap()).unwrap();
            let source_bytes = fs::read(&source).unwrap();
            let target = root.join("new.json");
            fs::write(&target, b"valuable").unwrap();
            let mut preflight = Progress {
                source: source.clone(),
                target: target.clone(),
                operation_id: Ulid::new(),
                phase: "preflight",
                db_id: None,
                source_lease: None,
                target_lease: None,
                bodies_created: false,
                target_published: false,
                backend: PredecessorBackend::Json,
                target_generation: TargetGeneration::SingleGraphV1,
                tag_export: None,
                tag_plan: None,
            };
            assert!(
                tokio::runtime::Builder::new_current_thread()
                    .enable_all()
                    .build()
                    .unwrap()
                    .block_on(upgrade(&mut preflight, PredecessorBackend::Json))
                    .is_err()
            );
            assert!(!preflight.target_published);
            assert_eq!(fs::read(&target).unwrap(), b"valuable");
            fs::remove_file(&target).unwrap();
            let mut after = Progress {
                target: target.clone(),
                operation_id: Ulid::new(),
                ..preflight
            };
            BODY_FAILURE.with(|slot| slot.set(Some("after_json_publish")));
            let error = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .unwrap()
                .block_on(upgrade(&mut after, PredecessorBackend::Json))
                .unwrap_err();
            assert!(error.to_string().contains("after_json_publish"));
            assert!(after.target_published);
            assert!(target.exists());
            assert_eq!(fs::read(&source).unwrap(), source_bytes);
            assert_eq!(
                MemStore::load_single_graph_v2(&target).unwrap().db_id(),
                old.db_id()
            );
            fs::remove_dir_all(root).unwrap();
        }
    }
}
