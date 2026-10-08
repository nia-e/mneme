use super::*;

#[derive(Clone, Debug)]
struct TaggedWorst(Scored);

impl PartialEq for TaggedWorst {
    fn eq(&self, other: &Self) -> bool {
        self.0.id == other.0.id && self.0.score.to_bits() == other.0.score.to_bits()
    }
}

impl Eq for TaggedWorst {}

impl PartialOrd for TaggedWorst {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

impl Ord for TaggedWorst {
    fn cmp(&self, other: &Self) -> Ordering {
        crate::scored_order(&self.0, &other.0)
    }
}

fn retain_cozo_tagged_top_k(heap: &mut BinaryHeap<TaggedWorst>, hit: Scored, k: usize) {
    if k == 0 {
        return;
    }
    let ranked = TaggedWorst(hit);
    if heap.len() < k {
        heap.push(ranked);
    } else if heap.peek().is_some_and(|worst| ranked < *worst) {
        *heap.peek_mut().expect("nonzero full tagged heap") = ranked;
    }
}

fn tagged_status(value: &str) -> Result<TaggedPhysicalStatus> {
    match value {
        "active" => Ok(TaggedPhysicalStatus::Active),
        "archived" => Ok(TaggedPhysicalStatus::Archived),
        other => Err(backend_str(format!(
            "tag projection contains unknown lifecycle {other:?}"
        ))),
    }
}

fn validate_tag_membership_row(
    row: &[DataValue],
    expected_tag: &str,
    expected_status: TaggedPhysicalStatus,
) -> Result<NodeId> {
    if row.len() != 4 {
        return Err(backend_str(format!(
            "tag membership scan returned {} columns; expected 4",
            row.len()
        )));
    }
    let tag = want_str(&row[0])?;
    let status = tagged_status(want_str(&row[1])?)?;
    let sample_hash = want_i64(&row[2])?;
    let id = node_id(want_str(&row[3])?)?;
    if tag != expected_tag || status != expected_status {
        return Err(backend_str(format!(
            "tag membership prefix scan escaped ({expected_tag:?}, {}): got ({tag:?}, {})",
            expected_status.as_str(),
            status.as_str()
        )));
    }
    let expected_hash = stable_tag_sample_hash(id);
    if sample_hash != expected_hash {
        return Err(backend_str(format!(
            "tag membership {} has sample hash {sample_hash}; expected {expected_hash}",
            id.0
        )));
    }
    Ok(id)
}

type TaggedMembershipEvidence = BTreeMap<NodeId, BTreeSet<(String, TaggedPhysicalStatus)>>;

fn exact_tag_memberships(
    tx: &mut BoundedReadTransaction,
    request: &TaggedAnnRequest<'_>,
    #[cfg(test)] test_hook: &TaggedReadTestHook,
) -> Result<(
    usize,
    TaggedMembershipEvidence,
    Option<TaggedExactWorkLimit>,
)> {
    let mut raw_memberships = 0usize;
    let mut memberships = TaggedMembershipEvidence::new();
    let unique_canary = request.limits.max_unique_exact_ids() + 1;

    'all_ranges: for tag in request.tags() {
        for status in TaggedPhysicalStatus::ALL {
            if !request
                .lanes
                .iter()
                .any(|lane| status.is_admitted_by(lane.status))
            {
                continue;
            }
            let mut after: Option<(i64, String)> = None;
            loop {
                let remaining = request.limits.max_raw_memberships() + 1 - raw_memberships;
                let limit = remaining.min(TAGGED_SCAN_PAGE);
                #[cfg(test)]
                let continued = after.is_some();
                let lower = after
                    .as_ref()
                    .map_or(PrimaryKeyScanBound::Unbounded, |key| {
                        PrimaryKeyScanBound::Excluded(vec![dv_int(key.0), dv_str(&key.1)])
                    });
                let rows = tx
                    .scan_relation_by_primary_key(
                        "node_tag_v2",
                        PrimaryKeyScan {
                            prefix: vec![dv_str(tag), dv_str(status.as_str())],
                            lower,
                            upper: PrimaryKeyScanBound::Unbounded,
                            direction: PrimaryKeyScanDirection::Ascending,
                            limit,
                        },
                    )
                    .map_err(backend)?;
                #[cfg(test)]
                test_hook.record_scan(tag, status, continued, limit, rows.rows.len());
                if rows.rows.len() > limit {
                    return Err(backend_str(
                        "bounded tag membership scan exceeded its requested page".into(),
                    ));
                }
                if rows.rows.is_empty() {
                    break;
                }
                for row in &rows.rows {
                    let id = validate_tag_membership_row(row, tag, status)?;
                    raw_memberships += 1;
                    if memberships.len() < unique_canary || memberships.contains_key(&id) {
                        memberships
                            .entry(id)
                            .or_default()
                            .insert(((*tag).to_owned(), status));
                    }
                    after = Some((want_i64(&row[2])?, want_str(&row[3])?.to_owned()));
                    if raw_memberships > request.limits.max_raw_memberships() {
                        break 'all_ranges;
                    }
                }
                if rows.rows.len() < limit {
                    break;
                }
            }
        }
    }

    let overflow = tagged_exact_work_overflow(
        raw_memberships,
        memberships.len(),
        request.query.len(),
        request.limits,
    )?;
    Ok((raw_memberships, memberships, overflow))
}

struct TaggedHydration {
    nodes: BTreeMap<NodeId, Node>,
    vectors: BTreeMap<NodeId, (Vec<f32>, TaggedPhysicalStatus)>,
    memberships: TaggedMembershipEvidence,
}

pub(super) fn tagged_membership_hydration_query(wanted: &str) -> String {
    format!(
        "{wanted}\n\
         ?[id, tag, status, sample_hash] := wanted_tagged[id], \
           *node_tag_v2:by_id{{id, tag, status, sample_hash}} \
           :limit $membership_cap"
    )
}

fn hydrate_tagged_ids(
    tx: &mut BoundedReadTransaction,
    ids: &[NodeId],
    include_all_memberships: bool,
) -> Result<TaggedHydration> {
    let mut nodes = BTreeMap::new();
    let mut vectors = BTreeMap::new();
    let mut memberships = TaggedMembershipEvidence::new();
    if ids.is_empty() {
        return Ok(TaggedHydration {
            nodes,
            vectors,
            memberships,
        });
    }
    let (wanted, params) = id_input("wanted_tagged", ids);
    let rows = tx
        .run_script(
            &format!(
                "{wanted}\n?[id, data, status] := wanted_tagged[id], *node{{id, data, status}}"
            ),
            params.clone(),
        )
        .map_err(backend)?;
    for row in &rows.rows {
        let node = decode_canonical_node_row(row)?;
        if nodes.insert(node.id(), node).is_some() {
            return Err(backend_str(
                "tagged hydration returned a duplicate node".into(),
            ));
        }
    }
    if nodes.len() != ids.len() {
        return Err(backend_str(format!(
            "tag projection/vector candidate references {} missing canonical nodes",
            ids.len() - nodes.len()
        )));
    }

    let rows = tx
        .run_script(
            &format!("{wanted}\n?[id, e, status] := wanted_tagged[id], *node_vec{{id, e, status}}"),
            params.clone(),
        )
        .map_err(backend)?;
    for row in &rows.rows {
        let id = node_id(want_str(&row[0])?)?;
        let vector = want_vector(&row[1])?;
        let status = tagged_status(want_str(&row[2])?)?;
        if vectors.insert(id, (vector, status)).is_some() {
            return Err(backend_str(
                "tagged hydration returned a duplicate vector".into(),
            ));
        }
    }

    if include_all_memberships {
        let cap = ids
            .len()
            .checked_mul(mneme_core::MAX_NODE_TAGS)
            .and_then(|value| value.checked_add(1))
            .ok_or_else(|| backend_str("tagged membership hydration cap overflowed".into()))?;
        let mut membership_params = params;
        membership_params.insert("membership_cap".into(), dv_int(cap as i64));
        let rows = tx
            .run_script(
                &tagged_membership_hydration_query(&wanted),
                membership_params,
            )
            .map_err(backend)?;
        if rows.rows.len() == cap {
            return Err(backend_str(
                "tagged membership hydration exceeded the canonical per-node bound".into(),
            ));
        }
        for row in &rows.rows {
            if row.len() != 4 {
                return Err(backend_str(
                    "tagged by-id hydration returned a malformed row".into(),
                ));
            }
            let id = node_id(want_str(&row[0])?)?;
            if !nodes.contains_key(&id) {
                return Err(backend_str(
                    "tagged by-id hydration escaped its candidate input".into(),
                ));
            }
            let tag = want_str(&row[1])?.to_owned();
            let status = tagged_status(want_str(&row[2])?)?;
            let sample_hash = want_i64(&row[3])?;
            if sample_hash != stable_tag_sample_hash(id) {
                return Err(backend_str(format!(
                    "tag membership {} has an invalid stable sample hash",
                    id.0
                )));
            }
            if !memberships.entry(id).or_default().insert((tag, status)) {
                return Err(backend_str(
                    "tagged by-id hydration returned a duplicate membership".into(),
                ));
            }
        }
        for (&id, node) in &nodes {
            let status = TaggedPhysicalStatus::from(node.status());
            let expected: BTreeSet<_> = node.tags().map(|tag| (tag.to_owned(), status)).collect();
            let actual = memberships.get(&id).cloned().unwrap_or_default();
            if actual != expected {
                return Err(backend_str(format!(
                    "tag projection differs from canonical tags for node {}",
                    id.0
                )));
            }
        }
    }

    Ok(TaggedHydration {
        nodes,
        vectors,
        memberships,
    })
}

fn exact_cozo_tagged_batch(
    tx: &mut BoundedReadTransaction,
    request: &TaggedAnnRequest<'_>,
    raw_memberships: usize,
    memberships: TaggedMembershipEvidence,
    generation: TaggedProjectionGeneration,
) -> Result<TaggedAnnBatch> {
    let ids: Vec<_> = memberships.keys().copied().collect();
    let unique_exact_ids = ids.len();
    let mut heaps: Vec<BinaryHeap<TaggedWorst>> = request
        .lanes
        .iter()
        .map(|lane| BinaryHeap::with_capacity(lane.k))
        .collect();
    let mut exact_hydrated_ids = 0usize;

    for page in ids.chunks(request.limits.exact_hydration_page_ids()) {
        let hydrated = hydrate_tagged_ids(tx, page, false)?;
        for id in page {
            let node = hydrated
                .nodes
                .get(id)
                .ok_or_else(|| backend_str(format!("tagged exact hydration lost node {}", id.0)))?;
            let canonical_status = TaggedPhysicalStatus::from(node.status());
            let evidence = memberships.get(id).expect("id originates in evidence");
            if evidence
                .iter()
                .any(|(tag, status)| *status != canonical_status || !node.has_tag(tag.as_str()))
            {
                return Err(backend_str(format!(
                    "tag projection membership disagrees with canonical node {}",
                    id.0
                )));
            }
            let Some((vector, vector_status)) = hydrated.vectors.get(id) else {
                continue;
            };
            if *vector_status != canonical_status {
                return Err(backend_str(format!(
                    "vector lifecycle disagrees with canonical node {}",
                    id.0
                )));
            }
            if vector.len() != request.query.len() {
                return Err(Error::DimMismatch {
                    index: request.query.len(),
                    provider: vector.len(),
                });
            }
            crate::validate_cosine_vector(vector, "stored tagged vector embedding")?;
            let lane_index = request
                .lanes
                .iter()
                .position(|lane| lane.status.allows(node.status()))
                .ok_or_else(|| {
                    backend_str(format!(
                        "tagged exact node {} escaped requested lifecycle lanes",
                        id.0
                    ))
                })?;
            exact_hydrated_ids += 1;
            retain_cozo_tagged_top_k(
                &mut heaps[lane_index],
                Scored {
                    id: *id,
                    score: crate::cosine(request.query, vector).clamp(-1.0, 1.0),
                },
                request.lanes[lane_index].k,
            );
        }
    }

    let lanes = request
        .lanes
        .iter()
        .zip(heaps)
        .map(|(requested, heap)| {
            let mut hits: Vec<_> = heap.into_iter().map(|ranked| ranked.0).collect();
            hits.sort_by(crate::scored_order);
            TaggedAnnLane {
                lane: requested.lane,
                hits,
                seed_coverage: TaggedSeedCoverage::ExactCosine,
            }
        })
        .collect();
    let exact_vector_components = request
        .limits
        .checked_exact_components(exact_hydrated_ids, request.query.len())?;
    TaggedAnnBatch::new(
        lanes,
        TaggedAnnWork {
            query_dimension: request.query.len(),
            raw_memberships,
            unique_exact_ids,
            exact_hydrated_ids,
            exact_vector_components,
            ..TaggedAnnWork::default()
        },
        generation,
    )
}

#[derive(Default)]
struct FallbackCandidateEvidence {
    physical: BTreeSet<TaggedPhysicalStatus>,
    sampled: BTreeSet<(String, TaggedPhysicalStatus)>,
    hnsw: BTreeSet<TaggedPhysicalStatus>,
}

struct FallbackLaneBuild {
    requested: TaggedAnnLaneRequest,
    candidates: BTreeMap<NodeId, FallbackCandidateEvidence>,
    physical: Vec<TaggedPhysicalSeedCoverage>,
}

fn vector_index_for_status(status: TaggedPhysicalStatus) -> &'static str {
    match status {
        TaggedPhysicalStatus::Active => ACTIVE_VECTOR_INDEX,
        TaggedPhysicalStatus::Archived => ARCHIVED_VECTOR_INDEX,
    }
}

fn fallback_hnsw_candidates(
    tx: &mut BoundedReadTransaction,
    request: &TaggedAnnRequest<'_>,
    status: TaggedPhysicalStatus,
    quota: usize,
) -> Result<Vec<NodeId>> {
    if quota == 0 {
        return Ok(Vec::new());
    }
    let quota = i64::try_from(quota)
        .map_err(|_| Error::InvalidInput("tagged HNSW quota exceeds i64".into()))?;
    let mut params = BTreeMap::new();
    params.insert("q".into(), dv_float_list(request.query));
    params.insert("k".into(), dv_int(quota));
    params.insert("ef".into(), dv_int(ANN_EF.max(quota)));
    let rows = tx
        .run_script(
            &format!(
                "?[id, dist] := ~node_vec:{}{{id | query: v, k: $k, ef: $ef, bind_distance: dist}}, \
                   v = vec($q) :order dist, id :limit $k",
                vector_index_for_status(status)
            ),
            params,
        )
        .map_err(backend)?;
    if rows.rows.len() > quota as usize {
        return Err(backend_str(
            "tagged HNSW returned more candidates than requested".into(),
        ));
    }
    let mut ids = Vec::with_capacity(rows.rows.len());
    let mut seen = HashSet::with_capacity(rows.rows.len());
    for row in &rows.rows {
        let id = node_id(want_str(&row[0])?)?;
        let distance = want_f64(&row[1])?;
        if !distance.is_finite() || !(0.0..=2.0).contains(&distance) {
            return Err(backend_str(format!(
                "tagged HNSW returned invalid cosine distance {distance:?}"
            )));
        }
        if !seen.insert(id) {
            return Err(backend_str("tagged HNSW returned a duplicate ID".into()));
        }
        ids.push(id);
    }
    Ok(ids)
}

fn fallback_sample_members(
    tx: &mut BoundedReadTransaction,
    tag: &str,
    status: TaggedPhysicalStatus,
    pivot: i64,
    quota: usize,
) -> Result<Vec<NodeId>> {
    if quota == 0 {
        return Ok(Vec::new());
    }
    let start = vec![dv_int(pivot), dv_str("")];
    let mut rows = tx
        .scan_relation_by_primary_key(
            "node_tag_v2",
            PrimaryKeyScan {
                prefix: vec![dv_str(tag), dv_str(status.as_str())],
                lower: PrimaryKeyScanBound::Included(start.clone()),
                upper: PrimaryKeyScanBound::Unbounded,
                direction: PrimaryKeyScanDirection::Ascending,
                limit: quota,
            },
        )
        .map_err(backend)?
        .rows;
    if rows.len() < quota {
        let wrapped = tx
            .scan_relation_by_primary_key(
                "node_tag_v2",
                PrimaryKeyScan {
                    prefix: vec![dv_str(tag), dv_str(status.as_str())],
                    lower: PrimaryKeyScanBound::Unbounded,
                    upper: PrimaryKeyScanBound::Excluded(start),
                    direction: PrimaryKeyScanDirection::Ascending,
                    limit: quota - rows.len(),
                },
            )
            .map_err(backend)?;
        rows.extend(wrapped.rows);
    }
    if rows.len() > quota {
        return Err(backend_str(
            "tagged hash sample exceeded its assigned quota".into(),
        ));
    }
    rows.iter()
        .map(|row| validate_tag_membership_row(row, tag, status))
        .collect()
}

fn fallback_cozo_tagged_batch(
    tx: &mut BoundedReadTransaction,
    request: &TaggedAnnRequest<'_>,
    raw_memberships: usize,
    unique_exact_ids: usize,
    exceeded_limit: TaggedExactWorkLimit,
    retrieval_generation: &str,
    projection_generation: TaggedProjectionGeneration,
) -> Result<TaggedAnnBatch> {
    let strategy = TaggedFallbackStrategy::LifecycleHnswAndHashedTagSample;
    let mut lane_builds = Vec::with_capacity(request.lanes.len());
    let mut all_candidates = BTreeSet::new();
    let mut fallback_hnsw_inspected = 0usize;
    let mut fallback_sample_inspected = 0usize;

    for &requested in request.lanes {
        let mut candidates = BTreeMap::<NodeId, FallbackCandidateEvidence>::new();
        let mut physical_coverage = Vec::new();
        for physical_quota in
            tagged_physical_status_quotas(request, requested.lane, retrieval_generation)?
        {
            let legs = tagged_fallback_leg_quotas(physical_quota.candidates, strategy)?;
            let hnsw = fallback_hnsw_candidates(tx, request, physical_quota.status, legs.hnsw)?;
            fallback_hnsw_inspected += hnsw.len();
            for id in hnsw.iter().copied() {
                let evidence = candidates.entry(id).or_default();
                evidence.physical.insert(physical_quota.status);
                evidence.hnsw.insert(physical_quota.status);
                all_candidates.insert(id);
            }
            let pivot = tagged_sample_pivot(
                request,
                physical_quota.status,
                requested.lane,
                retrieval_generation,
            )?;
            let mut tag_samples = Vec::with_capacity(request.tags().len());
            for (index, tag_quota) in tagged_query_tag_quotas(
                request,
                physical_quota.status,
                requested.lane,
                retrieval_generation,
                legs.sample,
            )?
            .into_iter()
            .enumerate()
            {
                let sampled = fallback_sample_members(
                    tx,
                    tag_quota.tag,
                    physical_quota.status,
                    pivot,
                    tag_quota.candidates,
                )?;
                fallback_sample_inspected += sampled.len();
                for id in sampled.iter().copied() {
                    let evidence = candidates.entry(id).or_default();
                    evidence.physical.insert(physical_quota.status);
                    evidence
                        .sampled
                        .insert((tag_quota.tag.to_owned(), physical_quota.status));
                    all_candidates.insert(id);
                }
                tag_samples.push(TaggedQueryTagSeedCoverage::new(
                    u8::try_from(index).expect("tag count is bounded below u8"),
                    tag_quota.candidates,
                    sampled.len(),
                )?);
            }
            physical_coverage.push(TaggedPhysicalSeedCoverage::new(
                physical_quota.status,
                legs.hnsw,
                hnsw.len(),
                tag_samples,
                pivot,
            )?);
        }
        lane_builds.push(FallbackLaneBuild {
            requested,
            candidates,
            physical: physical_coverage,
        });
    }

    let ids: Vec<_> = all_candidates.iter().copied().collect();
    if ids.len() > request.limits.max_fallback_hydration_ids() {
        return Err(backend_str(
            "tagged fallback candidate union exceeds its hydration allowance".into(),
        ));
    }
    let hydrated = hydrate_tagged_ids(tx, &ids, true)?;
    let mut fallback_hydrated_ids = 0usize;
    for (&id, (vector, vector_status)) in &hydrated.vectors {
        let node = hydrated
            .nodes
            .get(&id)
            .ok_or_else(|| backend_str(format!("tagged fallback vector {} is orphaned", id.0)))?;
        if *vector_status != TaggedPhysicalStatus::from(node.status()) {
            return Err(backend_str(format!(
                "tagged fallback vector lifecycle disagrees for node {}",
                id.0
            )));
        }
        if vector.len() != request.query.len() {
            return Err(Error::DimMismatch {
                index: request.query.len(),
                provider: vector.len(),
            });
        }
        crate::validate_cosine_vector(vector, "stored tagged fallback vector embedding")?;
        fallback_hydrated_ids += 1;
    }

    let mut lanes = Vec::with_capacity(lane_builds.len());
    let mut fallback_canonical_candidates_checked = 0usize;
    let mut fallback_matching_candidates = 0usize;
    for build in lane_builds {
        let canonical_candidates_checked = build.candidates.len();
        let mut matching_candidates = 0usize;
        let mut hits = Vec::new();
        for (id, evidence) in build.candidates {
            let node = hydrated.nodes.get(&id).expect("all candidates hydrated");
            let canonical_status = TaggedPhysicalStatus::from(node.status());
            if evidence.sampled.iter().any(|membership| {
                !hydrated
                    .memberships
                    .get(&id)
                    .is_some_and(|memberships| memberships.contains(membership))
            }) {
                return Err(backend_str(format!(
                    "sampled tag membership disappeared for node {}",
                    id.0
                )));
            }
            let has_requested_membership = request.tags().iter().any(|tag| {
                hydrated.memberships.get(&id).is_some_and(|memberships| {
                    memberships.contains(&((*tag).to_owned(), canonical_status))
                })
            });
            if !evidence.physical.contains(&canonical_status)
                || !build.requested.status.allows(node.status())
                || !request.tags().iter().any(|tag| node.has_tag(tag))
                || !has_requested_membership
            {
                continue;
            }
            matching_candidates += 1;
            let Some((vector, _)) = hydrated.vectors.get(&id) else {
                if evidence.hnsw.contains(&canonical_status) {
                    return Err(backend_str(format!(
                        "HNSW candidate {} has no canonical vector row",
                        id.0
                    )));
                }
                continue;
            };
            hits.push(Scored {
                id,
                score: crate::cosine(request.query, vector).clamp(-1.0, 1.0),
            });
        }
        hits.sort_by(crate::scored_order);
        hits.truncate(build.requested.k);
        fallback_canonical_candidates_checked += canonical_candidates_checked;
        fallback_matching_candidates += matching_candidates;
        lanes.push(TaggedAnnLane {
            lane: build.requested.lane,
            hits,
            seed_coverage: TaggedSeedCoverage::LifecycleHnswAndHashedTagSamplePostfilter {
                exceeded_limit,
                raw_memberships,
                physical: build.physical,
                canonical_candidates_checked,
                matching_candidates,
            },
        });
    }

    let fallback_vector_components = request
        .limits
        .checked_fallback_components(fallback_hydrated_ids, request.query.len())?;
    TaggedAnnBatch::new(
        lanes,
        TaggedAnnWork {
            query_dimension: request.query.len(),
            raw_memberships,
            unique_exact_ids,
            exact_hydrated_ids: 0,
            exact_vector_components: 0,
            fallback_hnsw_inspected,
            fallback_sample_inspected,
            fallback_unique_ids: ids.len(),
            fallback_canonical_candidates_checked,
            fallback_matching_candidates,
            fallback_hydrated_ids,
            fallback_vector_components,
        },
        projection_generation,
    )
}

fn cozo_tagged_batch(
    db: &DbInstance,
    request: &TaggedAnnRequest<'_>,
    retrieval_generation: &str,
    #[cfg(test)] test_hook: &TaggedReadTestHook,
) -> Result<TaggedAnnBatch> {
    let mut tx = db
        .read_multi_transaction_with_timeout(TAGGED_READ_TIMEOUT)
        .map_err(backend)?;
    #[cfg(test)]
    test_hook.block_if_held();
    let result = (|| {
        let mut params = BTreeMap::new();
        params.insert("key".into(), dv_str(crate::tag_projection::META_KEY));
        let marker = tx
            .run_script("?[value] := *meta{k: $key, v: value}", params)
            .map_err(backend)?;
        if marker.rows.len() != 1
            || marker.rows[0].len() != 1
            || want_str(&marker.rows[0][0])? != crate::tag_projection::META_VALUE
        {
            return Err(backend_str(
                "tagged retrieval requires the verified current tag projection marker".into(),
            ));
        }
        let generation = TaggedProjectionGeneration::new(crate::tag_projection::META_VALUE)?;
        let (raw_memberships, memberships, overflow) = exact_tag_memberships(
            &mut tx,
            request,
            #[cfg(test)]
            test_hook,
        )?;
        match overflow {
            None => {
                exact_cozo_tagged_batch(&mut tx, request, raw_memberships, memberships, generation)
            }
            Some(exceeded_limit) => {
                // The exact prefix can hold thousands of IDs. Once a fuse has
                // selected bounded fallback, retain only its charged cardinality
                // so exact and fallback allocations do not overlap at peak.
                let unique_exact_ids = memberships.len();
                drop(memberships);
                fallback_cozo_tagged_batch(
                    &mut tx,
                    request,
                    raw_memberships,
                    unique_exact_ids,
                    exceeded_limit,
                    retrieval_generation,
                    generation,
                )
            }
        }
    })();
    let close = tx.close_and_join().map_err(backend);
    match (result, close) {
        (Ok(batch), Ok(())) => Ok(batch),
        (Err(error), Ok(())) => Err(error),
        (Ok(_), Err(error)) => Err(error),
        (Err(error), Err(close_error)) => Err(backend_str(format!(
            "tagged read failed ({error}); transaction teardown also failed ({close_error})"
        ))),
    }
}

#[async_trait]
impl VectorIndex for CozoStore {
    fn semantic_id(&self) -> &'static str {
        "mnestic-0.13-hnsw-cosine-semantic-status-v2"
    }

    fn dim(&self) -> usize {
        self.dim
    }

    fn tagged_projection_generation(&self) -> Option<&'static str> {
        Some(crate::tag_projection::META_VALUE)
    }

    async fn upsert(&self, id: NodeId, embedding: &[f32]) -> Result<()> {
        if embedding.len() != self.dim {
            return Err(Error::DimMismatch {
                index: self.dim,
                provider: embedding.len(),
            });
        }
        crate::validate_cosine_vector(embedding, "vector embedding")?;
        let mut p = BTreeMap::new();
        p.insert("id".into(), dv_str(&id.0.to_string()));
        p.insert("e".into(), dv_float_list(embedding));
        // Read lifecycle and write the projection in one transaction. Besides
        // keeping a concurrent status transition coherent, the explicit read
        // lets this adapter honour VectorIndex's exact NotFound contract instead
        // of leaking Cozo's generic `:assert` backend error.
        let tx = self.db.multi_transaction(true);
        let staged = (|| {
            let node = episodes::tx_node(&tx, id)?.ok_or(Error::NotFound)?;
            if !node.is_semantic() {
                return Err(Error::InvalidInput(
                    "episode edition vectors are immutable outside detached construction".into(),
                ));
            }
            p.insert("status".into(), dv_str(status_str(node.status())));
            tx_run(
                &tx,
                "?[id, e, status] := id = $id, e = vec($e), status = $status \
                   :put node_vec {id => e, status}",
                p,
            )?;
            Ok::<_, Error>(())
        })();
        match staged {
            Ok(()) => tx.commit().map_err(backend),
            Err(error) => {
                let _ = tx.abort();
                Err(error)
            }
        }
    }

    async fn remove(&self, id: NodeId) -> Result<()> {
        let tx = self.db.multi_transaction(true);
        let result = (|| {
            tx_run(
                &tx,
                "?[k,v] := *meta{k,v}, k == 'db_id' :put meta {k=>v}",
                BTreeMap::new(),
            )?;
            if episodes::tx_node(&tx, id)?.is_some_and(|node| !node.is_semantic()) {
                return Err(Error::InvalidInput(
                    "episode edition vectors are immutable".into(),
                ));
            }
            let mut p = BTreeMap::new();
            p.insert("id".into(), dv_str(&id.0.to_string()));
            tx_run(
                &tx,
                "?[id] := *node_vec{id}, id == $id :rm node_vec {id}",
                p,
            )?;
            Ok(())
        })();
        match result {
            Ok(()) => tx.commit().map_err(backend),
            Err(error) => {
                let _ = tx.abort();
                Err(error)
            }
        }
    }

    async fn ann(&self, query: &[f32], k: usize, status: StatusFilter) -> Result<Vec<Scored>> {
        if query.len() != self.dim {
            return Err(Error::DimMismatch {
                index: self.dim,
                provider: query.len(),
            });
        }
        crate::validate_cosine_vector(query, "ANN query vector")?;
        if !status.active && !status.archived {
            return Ok(Vec::new());
        }
        if k == 0 {
            return Ok(Vec::new());
        }

        let lanes = selected_vector_lanes(status);
        let k_i64 = i64::try_from(k)
            .map_err(|_| Error::InvalidInput("ANN k exceeds the storage integer range".into()))?;

        // Ask each disjoint lifecycle graph for k. An item outside its lane's
        // top-k cannot enter the union's top-k because k same-lane items already
        // precede it. One Cozo script gives every lane the same read snapshot.
        let mut rules = Vec::with_capacity(lanes.len() + 1);
        for (index, _) in lanes {
            rules.push(format!(
                "lane[id, dist] := ~node_vec:{index}{{id | query: v, k: $k, ef: $ef, bind_distance: dist}}, v = vec($q)"
            ));
        }
        rules.push("?[id, dist] := lane[id, dist] :order dist, id :limit $k".into());
        let mut p = BTreeMap::new();
        p.insert("q".into(), dv_float_list(query));
        p.insert("k".into(), dv_int(k_i64));
        p.insert("ef".into(), dv_int(ANN_EF.max(k_i64)));
        scored_rows(self.run_async(rules.join("\n"), p, false).await?, k)
    }

    async fn tagged_ann(&self, request: TaggedAnnRequest<'_>) -> Result<TaggedAnnBatch> {
        request.validate()?;
        if request.query.len() != self.dim {
            return Err(Error::DimMismatch {
                index: self.dim,
                provider: request.query.len(),
            });
        }
        crate::validate_cosine_vector(request.query, "tagged ANN query vector")?;
        let permit = self.tagged_read_admission.try_acquire()?;
        let query = request.query.to_vec();
        let tags: Vec<String> = request.tags().iter().map(|tag| (*tag).to_owned()).collect();
        let lanes = request.lanes.to_vec();
        let limits = request.limits;
        let retrieval_generation = <Self as VectorIndex>::semantic_id(self);
        let job = ActiveBackendJob {
            db: self.db.clone(),
            authority: self.persistent_authority.clone(),
            activity: self.backend_activity.enter(),
        };
        #[cfg(test)]
        let test_hook = self.tagged_read_test_hook.clone();
        let batch = tokio::task::spawn_blocking(move || {
            let _permit = permit;
            job.run(|db| {
                let tag_refs: Vec<&str> = tags.iter().map(String::as_str).collect();
                let request = TaggedAnnRequest::new(&query, tag_refs, &lanes, limits)?;
                cozo_tagged_batch(
                    db,
                    &request,
                    retrieval_generation,
                    #[cfg(test)]
                    &test_hook,
                )
            })
        })
        .await
        .map_err(|error| backend_str(format!("join tagged ANN worker: {error}")))??;
        batch.validate_against(
            &request,
            retrieval_generation,
            crate::tag_projection::META_VALUE,
        )?;
        Ok(batch)
    }
}
