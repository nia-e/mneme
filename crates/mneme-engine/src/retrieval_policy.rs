//! Retrieval policy identity, separate from executing a search.
//!
//! Ranking/admission inputs and enabled backend semantics determine the policy
//! fingerprint. Embedding identity and verified projection generations remain
//! separate stamp components; observed search work never becomes policy.

use mneme_core::EmbeddingFingerprint;
use mneme_core::ports::{Budget, StatusFilter};
use mneme_core::tagged::{
    RetrievalLifecycleLane, TaggedAnnLane, TaggedAnnRequest, TaggedSeedCoverage,
};

use crate::{Config, MAX_LEXICAL_K, MAX_RETRIEVAL_RANK, Memory};

pub const MAX_PROJECTION_WATERMARKS: usize = 8;
const TAGGED_RETRIEVAL_POLICY_GENERATION: &str = "tagged-seed-v3";

/// Version of the rank-only retrieval policy. The string is deliberately
/// opaque to consumers: compare it for equality and treat a change as a new
/// ordering/admission contract.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct RetrievalPolicyVersion {
    /// Human-readable contract generation.
    pub contract: &'static str,
    /// SHA-256 over every runtime knob and enabled leg that changes ranking or
    /// result admission under this contract.
    pub fingerprint: [u8; 32],
}

impl RetrievalPolicyVersion {
    pub const fn as_str(&self) -> &'static str {
        self.contract
    }
}

/// Runtime identity of the retrieval indexes visible to this engine. This is a
/// structured transitional fingerprint: the embedding contract is exact, and
/// enabled retrieval legs expose stable semantic generations rather than
/// booleans that would equate behavior-changing backend upgrades. Physical
/// projection generations remain separate below.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct IndexSetFingerprint {
    pub embedding: EmbeddingFingerprint,
    pub vector_semantics: &'static str,
    pub lexical_semantics: Option<&'static str>,
    pub reranker_semantics: Option<&'static str>,
}

/// One backend-supplied projection watermark. Tagged retrieval supplies the
/// verified tag-membership generation; paths without a trustworthy generation
/// leave the bounded list empty rather than manufacturing one.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ProjectionWatermark {
    pub projection: String,
    pub watermark: String,
}

/// Construction-validated projection watermarks. The private storage makes the
/// eight-item protocol limit an invariant instead of a comment on a public
/// `Vec` that any caller could violate after construction.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ProjectionWatermarks(Vec<ProjectionWatermark>);

impl ProjectionWatermarks {
    pub fn new(values: Vec<ProjectionWatermark>) -> std::result::Result<Self, WatermarkLimitError> {
        if values.len() > MAX_PROJECTION_WATERMARKS {
            return Err(WatermarkLimitError {
                provided: values.len(),
            });
        }
        Ok(Self(values))
    }

    pub const fn as_slice(&self) -> &[ProjectionWatermark] {
        self.0.as_slice()
    }

    pub const fn is_empty(&self) -> bool {
        self.0.is_empty()
    }

    pub const fn len(&self) -> usize {
        self.0.len()
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct WatermarkLimitError {
    pub provided: usize,
}

impl std::fmt::Display for WatermarkLimitError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            formatter,
            "retrieval policy stamp has {} projection watermarks; maximum is {MAX_PROJECTION_WATERMARKS}",
            self.provided
        )
    }
}

impl std::error::Error for WatermarkLimitError {}

/// Policy/index identity attached to one typed batch. At most eight projection
/// watermarks may be carried. Untagged retrieval currently carries none; tagged
/// retrieval carries the adapter's verified `tag-membership` generation.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RetrievalPolicyStamp {
    pub retrieval_policy: RetrievalPolicyVersion,
    pub index_set: IndexSetFingerprint,
    pub projection_watermarks: ProjectionWatermarks,
}

/// Fixed-size identity of the backend behavior that can affect retrieval. A
/// configured but disabled optional leg is deliberately absent: it cannot have
/// influenced this batch, while enabling it produces a different component.
fn retrieval_index_semantics_fingerprint(
    vector: &str,
    lexical: Option<&str>,
    reranker: Option<&str>,
) -> [u8; 32] {
    use sha2::{Digest, Sha256};

    let mut hash = Sha256::new();
    hash.update(b"mneme:retrieval-index-semantics-v1\0");
    for semantic_id in [Some(vector), lexical, reranker] {
        match semantic_id {
            Some(value) => {
                hash.update([1]);
                hash.update((value.len() as u64).to_le_bytes());
                hash.update(value.as_bytes());
            }
            None => hash.update([0]),
        }
    }
    hash.finalize().into()
}

/// Fingerprint only the declared tagged strategy and quota topology. Observed
/// work (inspected/matching counts and pivots) belongs in result metadata, not
/// policy identity.
fn tagged_coverage_shape_fingerprint(lanes: &[TaggedAnnLane]) -> [u8; 32] {
    use sha2::{Digest, Sha256};

    let mut hash = Sha256::new();
    hash.update(b"mneme:tagged-coverage-shape:v1\0");
    hash.update((lanes.len() as u64).to_le_bytes());
    for lane in lanes {
        let strategy = lane.seed_coverage.strategy_id();
        hash.update((strategy.len() as u64).to_le_bytes());
        hash.update(strategy.as_bytes());
        let physical = match &lane.seed_coverage {
            TaggedSeedCoverage::ExactCosine => None,
            TaggedSeedCoverage::LifecycleHnswAndHashedTagSamplePostfilter { physical, .. }
            | TaggedSeedCoverage::DeterministicHashedTagSamplePostfilter { physical, .. } => {
                Some(physical.as_slice())
            }
        };
        match physical {
            None => hash.update([0]),
            Some(physical) => {
                hash.update([1]);
                hash.update((physical.len() as u64).to_le_bytes());
                for item in physical {
                    let status = item.status.as_str();
                    hash.update((status.len() as u64).to_le_bytes());
                    hash.update(status.as_bytes());
                    hash.update((item.hnsw_quota as u64).to_le_bytes());
                    hash.update((item.tag_samples.len() as u64).to_le_bytes());
                    for sample in &item.tag_samples {
                        hash.update([sample.query_tag_index]);
                        hash.update((sample.quota as u64).to_le_bytes());
                    }
                }
            }
        }
    }
    hash.finalize().into()
}

#[derive(Clone, Copy)]
pub(super) struct RetrievalPolicyInput<'a> {
    pub(super) effective_k: usize,
    pub(super) budget: Budget,
    pub(super) status: StatusFilter,
    pub(super) tags: &'a [&'a str],
    pub(super) sparse_enabled: bool,
    pub(super) reranker_enabled: bool,
    pub(super) tagged: Option<TaggedPolicyInput<'a>>,
}

/// A tagged policy identity is complete only with its request, validated
/// coverage, and projection watermark. They travel together or not at all;
/// observed work remains result metadata rather than policy identity.
#[derive(Clone, Copy)]
pub(super) struct TaggedPolicyInput<'a> {
    pub(super) request: &'a TaggedAnnRequest<'a>,
    pub(super) lanes: &'a [TaggedAnnLane],
    pub(super) projection_generation: &'a str,
}

impl Memory {
    pub(super) fn retrieval_policy_stamp(
        &self,
        input: RetrievalPolicyInput<'_>,
    ) -> RetrievalPolicyStamp {
        let projection_watermarks =
            input
                .tagged
                .map_or_else(ProjectionWatermarks::default, |tagged| {
                    ProjectionWatermarks::new(vec![ProjectionWatermark {
                        projection: "tag-membership".into(),
                        watermark: tagged.projection_generation.into(),
                    }])
                    .expect("one tagged projection watermark is within the domain limit")
                });
        let index_set = IndexSetFingerprint {
            embedding: self.embedder.fingerprint(),
            vector_semantics: self.vectors.semantic_id(),
            lexical_semantics: input.sparse_enabled.then(|| {
                self.lexical
                    .as_ref()
                    .expect("enabled lexical index")
                    .semantic_id()
            }),
            reranker_semantics: input.reranker_enabled.then(|| {
                self.reranker
                    .as_ref()
                    .expect("enabled reranker")
                    .semantic_id()
            }),
        };
        RetrievalPolicyStamp {
            retrieval_policy: input.version(&self.cfg, &index_set),
            index_set,
            projection_watermarks,
        }
    }
}

impl RetrievalPolicyInput<'_> {
    /// Only selected backend identities participate. Hashing is pure over the
    /// policy inputs; it neither queries adapters nor records observed work.
    fn version(self, cfg: &Config, index_set: &IndexSetFingerprint) -> RetrievalPolicyVersion {
        use sha2::{Digest, Sha256};

        let Self {
            effective_k,
            budget,
            status,
            tags,
            tagged,
            ..
        } = self;
        let sparse_enabled = index_set.lexical_semantics.is_some();
        let reranker_enabled = index_set.reranker_semantics.is_some();

        let mut hash = Sha256::new();
        hash.update(
            b"mneme:rank-only-v6\0semantic-corpus-only\0precise-graph-fusion-direct-winner-ties\0conditional-target-fair-entry-v1\0",
        );
        hash.update(retrieval_index_semantics_fingerprint(
            index_set.vector_semantics,
            index_set.lexical_semantics,
            index_set.reranker_semantics,
        ));
        for value in [
            cfg.lexical_k,
            cfg.graph_seed_cap,
            cfg.graph_slot_cap,
            MAX_LEXICAL_K,
            MAX_RETRIEVAL_RANK,
            effective_k,
            budget.max_nodes,
        ] {
            hash.update((value as u64).to_le_bytes());
        }
        for value in [
            cfg.rrf_constant,
            cfg.dense_weight,
            cfg.lexical_weight,
            cfg.graph_weight,
            budget.min_relevance,
            budget.explore,
            budget.relevance_ratio,
            budget.dedup_similarity,
            budget.query_conditioning,
        ] {
            hash.update(value.to_bits().to_le_bytes());
        }
        hash.update([budget.max_depth]);
        hash.update([
            u8::from(status.active),
            u8::from(status.archived),
            u8::from(sparse_enabled),
            u8::from(reranker_enabled),
        ]);
        hash.update((tags.len() as u64).to_le_bytes());
        for tag in tags {
            hash.update((tag.len() as u64).to_le_bytes());
            hash.update(tag.as_bytes());
        }
        match tagged {
            Some(TaggedPolicyInput { request, lanes, .. }) => {
                hash.update([1]);
                hash.update(TAGGED_RETRIEVAL_POLICY_GENERATION.as_bytes());
                for value in [
                    request.limits.max_raw_memberships(),
                    request.limits.max_unique_exact_ids(),
                    request.limits.max_exact_vector_components(),
                    request.limits.exact_hydration_page_ids(),
                    request.limits.max_fallback_candidates(),
                    request.limits.max_fallback_hydration_ids(),
                    request.limits.max_fallback_vector_components(),
                ] {
                    hash.update((value as u64).to_le_bytes());
                }
                hash.update((request.lanes.len() as u64).to_le_bytes());
                for lane in request.lanes {
                    hash.update([match lane.lane {
                        RetrievalLifecycleLane::Primary => 0,
                    }]);
                    hash.update([u8::from(lane.status.active), u8::from(lane.status.archived)]);
                    hash.update((lane.k as u64).to_le_bytes());
                    hash.update((lane.fallback_candidates as u64).to_le_bytes());
                }
                hash.update(tagged_coverage_shape_fingerprint(lanes));
            }
            None => hash.update([0]),
        }

        RetrievalPolicyVersion {
            contract: "rank-only-v6",
            fingerprint: hash.finalize().into(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;

    use mneme_core::ports::{Embedder, Reranker, Result};
    use mneme_core::tagged::{TaggedAnnLaneRequest, TaggedAnnWorkLimits};

    struct PolicyFixtureEmbedder;

    #[async_trait::async_trait]
    impl Embedder for PolicyFixtureEmbedder {
        fn dim(&self) -> usize {
            4
        }

        fn fingerprint(&self) -> EmbeddingFingerprint {
            EmbeddingFingerprint::new("ordering-fixture", 4, "l2-f32-v1", "symmetric-v1")
        }

        async fn embed(&self, _: &[&str]) -> Result<Vec<Vec<f32>>> {
            panic!("policy identity must not perform inference")
        }
    }

    struct PolicyFixtureReranker;

    #[async_trait::async_trait]
    impl Reranker for PolicyFixtureReranker {
        fn semantic_id(&self) -> &'static str {
            "policy-fixture-reranker-v1"
        }

        async fn rerank(&self, _: &str, _: &[&str]) -> Result<Vec<f32>> {
            panic!("policy identity must not perform inference")
        }
    }

    #[test]
    fn retrieval_policy_fingerprints_match_frozen_contract() {
        use mneme_core::ports::SystemClock;
        use mneme_core::tagged::{
            MAX_TAGGED_RAW_MEMBERSHIPS, TaggedExactWorkLimit, TaggedPhysicalSeedCoverage,
            TaggedPhysicalStatus, TaggedQueryTagSeedCoverage,
        };

        let store = Arc::new(mneme_cozo::MemStore::new(4));
        let memory = Memory::new(
            store.clone(),
            store.clone(),
            store.clone(),
            Arc::new(PolicyFixtureEmbedder),
            Arc::new(SystemClock),
            Config {
                lexical_k: 7,
                graph_seed_cap: 5,
                graph_slot_cap: 11,
                rrf_constant: 31.5,
                dense_weight: 0.75,
                lexical_weight: 1.25,
                graph_weight: 0.625,
                ..Config::default()
            },
        )
        .with_lexical_index(store)
        .with_reranker(Arc::new(PolicyFixtureReranker));
        let budget = Budget {
            max_nodes: 23,
            max_depth: 3,
            min_relevance: 0.125,
            explore: 0.25,
            relevance_ratio: 0.5,
            dedup_similarity: 0.875,
            query_conditioning: 0.375,
        };
        let input = RetrievalPolicyInput {
            effective_k: 13,
            budget,
            status: StatusFilter::ALL,
            tags: &[],
            sparse_enabled: false,
            reranker_enabled: false,
            tagged: None,
        };
        let check = |name: &str, input: RetrievalPolicyInput<'_>| {
            let stamp = memory.retrieval_policy_stamp(input);
            assert_eq!(stamp.retrieval_policy.as_str(), "rank-only-v6");
            assert_eq!(
                stamp.index_set.embedding,
                EmbeddingFingerprint::new("ordering-fixture", 4, "l2-f32-v1", "symmetric-v1")
            );
            assert_eq!(
                stamp.index_set.vector_semantics,
                "mneme-exact-cosine-semantic-status-v3"
            );
            assert_eq!(
                stamp.index_set.lexical_semantics,
                input
                    .sparse_enabled
                    .then_some("mneme-semantic-eligible-corpus-bm25-v2")
            );
            assert_eq!(
                stamp.index_set.reranker_semantics,
                input
                    .reranker_enabled
                    .then_some("policy-fixture-reranker-v1")
            );
            let expected_watermarks: Vec<_> = input
                .tagged
                .map(|tagged| tagged.projection_generation)
                .into_iter()
                .map(|generation| ProjectionWatermark {
                    projection: "tag-membership".into(),
                    watermark: generation.into(),
                })
                .collect();
            assert_eq!(stamp.projection_watermarks.as_slice(), expected_watermarks);
            let digest: String = stamp
                .retrieval_policy
                .fingerprint
                .iter()
                .map(|byte| format!("{byte:02x}"))
                .collect();
            // Captured from the pre-extraction implementation. These are byte-format
            // compatibility fixtures, not merely pairwise inequality checks.
            let expected = match name {
                "untagged-dense" => {
                    "61853bdc5b9a66ec5c9b249e7b737f295d3ab93d6e89e1bfd45b2724af26a48d"
                }
                "untagged-sparse" => {
                    "06939bc594f957443313bb18b130a2f50ef3f39f55966314fdfb029a26933b94"
                }
                "untagged-all-legs" => {
                    "82ea787b1effdf886e77cbf429322040e601e66bcb65dc3a45b064f568460fc3"
                }
                "tagged-exact" => {
                    "098aa0f752037b1b9739bf656988f2a7623cc11b394e5b52415871401b4882ad"
                }
                "tagged-deterministic" => {
                    "b95ad75550cdffacbb5bc5ba4b83e6088cf83a0af7a245a82bff338582551db2"
                }
                "tagged-hybrid" => {
                    "62f48ecbef05ae5bcf891c4930e0dc49b99210c76f0b2827c60264b09e0aa50a"
                }
                _ => unreachable!("unknown policy fixture"),
            };
            assert_eq!(digest, expected, "policy fixture {name}");
        };
        check("untagged-dense", input);
        check(
            "untagged-sparse",
            RetrievalPolicyInput {
                sparse_enabled: true,
                ..input
            },
        );
        check(
            "untagged-all-legs",
            RetrievalPolicyInput {
                sparse_enabled: true,
                reranker_enabled: true,
                ..input
            },
        );

        let query = [1.0, 0.0, 0.0, 0.0];
        let lanes = [TaggedAnnLaneRequest::new(
            RetrievalLifecycleLane::Primary,
            StatusFilter::ALL,
            13,
            192,
        )
        .unwrap()];
        let request = TaggedAnnRequest::new(
            &query,
            ["alpha", "βeta"],
            &lanes,
            TaggedAnnWorkLimits::default(),
        )
        .unwrap();
        let physical = |hnsw_quota, sample_quota| {
            TaggedPhysicalSeedCoverage::new(
                TaggedPhysicalStatus::Active,
                hnsw_quota,
                0,
                vec![TaggedQueryTagSeedCoverage::new(0, sample_quota, 0).unwrap()],
                0,
            )
            .unwrap()
        };
        let coverage = [
            ("tagged-exact", TaggedSeedCoverage::ExactCosine),
            (
                "tagged-deterministic",
                TaggedSeedCoverage::DeterministicHashedTagSamplePostfilter {
                    exceeded_limit: TaggedExactWorkLimit::RawMemberships,
                    raw_memberships: MAX_TAGGED_RAW_MEMBERSHIPS + 1,
                    physical: vec![physical(0, 192)],
                    canonical_candidates_checked: 0,
                    matching_candidates: 0,
                },
            ),
            (
                "tagged-hybrid",
                TaggedSeedCoverage::LifecycleHnswAndHashedTagSamplePostfilter {
                    exceeded_limit: TaggedExactWorkLimit::RawMemberships,
                    raw_memberships: MAX_TAGGED_RAW_MEMBERSHIPS + 1,
                    physical: vec![physical(96, 96)],
                    canonical_candidates_checked: 0,
                    matching_candidates: 0,
                },
            ),
        ];
        for (name, seed_coverage) in coverage {
            seed_coverage.validate().unwrap();
            let returned = [TaggedAnnLane {
                lane: RetrievalLifecycleLane::Primary,
                hits: Vec::new(),
                seed_coverage,
            }];
            check(
                name,
                RetrievalPolicyInput {
                    tags: request.tags(),
                    reranker_enabled: true,
                    tagged: Some(TaggedPolicyInput {
                        request: &request,
                        lanes: &returned,
                        projection_generation: "policy-fixture-projection-v1",
                    }),
                    ..input
                },
            );
        }
    }

    #[test]
    fn retrieval_backend_semantics_are_fingerprinted_only_when_enabled() {
        let fingerprint =
            |vector, lexical, lexical_enabled: bool, reranker, reranker_enabled: bool| {
                retrieval_index_semantics_fingerprint(
                    vector,
                    lexical_enabled.then_some(lexical),
                    reranker_enabled.then_some(reranker),
                )
            };
        let baseline = fingerprint("vector-v1", "lexical-v1", true, "reranker-v1", true);
        assert_ne!(
            baseline,
            fingerprint("vector-v2", "lexical-v1", true, "reranker-v1", true)
        );
        assert_ne!(
            baseline,
            fingerprint("vector-v1", "lexical-v2", true, "reranker-v1", true)
        );
        assert_ne!(
            baseline,
            fingerprint("vector-v1", "lexical-v1", true, "reranker-v2", true)
        );

        let disabled = fingerprint("vector-v1", "lexical-v1", false, "reranker-v1", false);
        assert_eq!(
            disabled,
            fingerprint("vector-v1", "lexical-v2", false, "reranker-v2", false),
            "configured but unused legs must not claim to influence the batch"
        );
    }

    #[test]
    fn tagged_policy_fingerprint_tracks_strategy_and_quotas_not_observed_work() {
        use mneme_core::tagged::{
            MAX_TAGGED_RAW_MEMBERSHIPS, TaggedExactWorkLimit, TaggedPhysicalSeedCoverage,
            TaggedPhysicalStatus, TaggedQueryTagSeedCoverage,
        };

        let lane = |coverage| TaggedAnnLane {
            lane: RetrievalLifecycleLane::Primary,
            hits: Vec::new(),
            seed_coverage: coverage,
        };
        let physical = |hnsw_quota, sample_quota, sample_inspected, pivot| {
            TaggedPhysicalSeedCoverage::new(
                TaggedPhysicalStatus::Active,
                hnsw_quota,
                0,
                vec![TaggedQueryTagSeedCoverage::new(0, sample_quota, sample_inspected).unwrap()],
                pivot,
            )
            .unwrap()
        };
        let partial = |hybrid, physical, checked, matching| {
            if hybrid {
                TaggedSeedCoverage::LifecycleHnswAndHashedTagSamplePostfilter {
                    exceeded_limit: TaggedExactWorkLimit::RawMemberships,
                    raw_memberships: MAX_TAGGED_RAW_MEMBERSHIPS + 1,
                    physical: vec![physical],
                    canonical_candidates_checked: checked,
                    matching_candidates: matching,
                }
            } else {
                TaggedSeedCoverage::DeterministicHashedTagSamplePostfilter {
                    exceeded_limit: TaggedExactWorkLimit::RawMemberships,
                    raw_memberships: MAX_TAGGED_RAW_MEMBERSHIPS + 1,
                    physical: vec![physical],
                    canonical_candidates_checked: checked,
                    matching_candidates: matching,
                }
            }
        };

        let exact = [lane(TaggedSeedCoverage::ExactCosine)];
        let deterministic = [lane(partial(false, physical(0, 192, 0, 0), 0, 0))];
        let deterministic_observed = [lane(partial(false, physical(0, 192, 1, 99), 1, 1))];
        let hybrid = [lane(partial(true, physical(96, 96, 0, 0), 0, 0))];
        let changed_quota = [lane(partial(true, physical(64, 128, 0, 0), 0, 0))];
        for lanes in [
            exact.as_slice(),
            deterministic.as_slice(),
            deterministic_observed.as_slice(),
            hybrid.as_slice(),
            changed_quota.as_slice(),
        ] {
            lanes[0].seed_coverage.validate().unwrap();
        }

        let digest = tagged_coverage_shape_fingerprint;
        assert_ne!(digest(&exact), digest(&deterministic));
        assert_ne!(digest(&deterministic), digest(&hybrid));
        assert_ne!(digest(&hybrid), digest(&changed_quota));
        assert_eq!(
            digest(&deterministic),
            digest(&deterministic_observed),
            "observed counters and pivots do not redefine retrieval policy"
        );
    }

    #[test]
    fn projection_watermark_count_is_construction_bounded() {
        let watermark = || ProjectionWatermark {
            projection: "dense".into(),
            watermark: "generation-1".into(),
        };
        let bounded = ProjectionWatermarks::new(
            (0..MAX_PROJECTION_WATERMARKS)
                .map(|_| watermark())
                .collect(),
        )
        .unwrap();
        assert_eq!(bounded.len(), MAX_PROJECTION_WATERMARKS);
        assert!(matches!(
            ProjectionWatermarks::new(
                (0..=MAX_PROJECTION_WATERMARKS)
                    .map(|_| watermark())
                    .collect()
            ),
            Err(WatermarkLimitError { provided }) if provided == MAX_PROJECTION_WATERMARKS + 1
        ));
    }
}
