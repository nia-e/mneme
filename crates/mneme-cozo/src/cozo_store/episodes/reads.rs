//! Snapshot-coherent episode reads. Timeline/history use native physical key
//! ranges; cue uses the lexical index, never a top-k semantic post-filter.

use super::*;

const TIMELINE_SCAN_ROWS: usize = 256;
const EPISODE_READ_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(5);

fn invalid(error: impl std::fmt::Display) -> Error {
    Error::InvalidInput(error.to_string())
}

impl CozoStore {
    async fn episode_read<T: Send + 'static>(
        &self,
        read: impl FnOnce(&mut BoundedReadTransaction, Ulid) -> Result<T> + Send + 'static,
    ) -> Result<T> {
        self.episode_read_with_remaining(EPISODE_READ_TIMEOUT, read)
            .await
    }

    async fn episode_read_with_remaining<T: Send + 'static>(
        &self,
        allowance: std::time::Duration,
        read: impl FnOnce(&mut BoundedReadTransaction, Ulid) -> Result<T> + Send + 'static,
    ) -> Result<T> {
        let started = std::time::Instant::now();
        let db_id = self.db_id;
        let job = ActiveBackendJob {
            db: self.db.clone(),
            authority: self.persistent_authority.clone(),
            activity: self.backend_activity.enter(),
        };
        tokio::task::spawn_blocking(move || {
            job.run(|db| {
                let remaining = crate::linked_read_remaining(started, allowance)?;
                let mut tx = db
                    .read_multi_transaction_with_timeout(remaining)
                    .map_err(backend)?;
                let result = ensure_read_generation(&mut tx, db_id)
                    .and_then(|()| read(&mut tx, db_id))
                    .and_then(|value| {
                        crate::linked_read_remaining(started, allowance)?;
                        Ok(value)
                    });
                let close = tx.close_and_join().map_err(backend);
                match (result, close) {
                    (Ok(value), Ok(())) => Ok(value),
                    (Err(error), Ok(())) | (Ok(_), Err(error)) => Err(error),
                    (Err(error), Err(close)) => Err(backend_str(format!(
                        "episode read failed ({error}); snapshot teardown failed ({close})"
                    ))),
                }
            })
        })
        .await
        .map_err(|error| backend_str(format!("join episode read worker: {error}")))?
    }

    pub(super) async fn episode_header_by_edition_impl(
        &self,
        request: &EpisodeHeaderByEditionRequest,
    ) -> Result<EpisodeHeaderByEdition> {
        request.validate()?;
        let edition = request.edition_id();
        self.episode_read_with_remaining(request.remaining(), move |tx, _| {
            let Some(node) = read_node(tx, edition)? else {
                return Ok(EpisodeHeaderByEdition::Missing);
            };
            let Some(facet) = node.episode() else {
                return Ok(EpisodeHeaderByEdition::Semantic);
            };
            let current = read_head(tx, facet.root())?
                .ok_or_else(|| backend_str("episode edition has no current head".into()))?;
            // Exact canonical edition + observed root head, under one snapshot.
            // Never hydrate the head or retarget a historical citation to it.
            Ok(EpisodeHeaderByEdition::Episode(header(&node, current)?))
        })
        .await
    }

    pub(super) async fn episode_get_impl(
        &self,
        request: &EpisodeGet,
    ) -> Result<Option<EpisodeRecord>> {
        let request = request.clone();
        self.episode_read(move |tx, _| {
            let Some(current) = read_head(tx, request.episode_id)? else {
                return Ok(None);
            };
            let edition = request.edition_id.unwrap_or(current);
            let Some(node) = read_node(tx, edition)? else {
                return if request.edition_id.is_some() {
                    Ok(None)
                } else {
                    Err(backend_str("episode head names a missing edition".into()))
                };
            };
            let facet = node
                .episode()
                .ok_or_else(|| invalid("requested node is not an episode edition"))?;
            if facet.root() != request.episode_id {
                return Err(invalid("requested edition does not belong to this episode"));
            }
            Ok(Some(EpisodeRecord {
                identity: identity(&node)?,
                node,
                current_edition_id: current,
            }))
        })
        .await
    }

    pub(super) async fn episode_timeline_impl(
        &self,
        request: &EpisodeTimelineRequest,
    ) -> Result<EpisodePage<EpisodeTimelineCursor>> {
        request.validate().map_err(invalid)?;
        if let Some(cursor) = &request.after {
            cursor.validate(self.db_id, request).map_err(invalid)?;
        }
        let request = request.clone();
        self.episode_read(move |tx, db| timeline(tx, db, &request))
            .await
    }

    pub(super) async fn episode_cue_impl(
        &self,
        request: &EpisodeCueRequest,
    ) -> Result<EpisodeCuePage> {
        request.validate().map_err(invalid)?;
        let request = request.clone();
        self.episode_read(move |tx, _| cue(tx, &request)).await
    }

    pub(super) async fn episode_history_impl(
        &self,
        request: &EpisodeHistoryRequest,
    ) -> Result<EpisodePage<EpisodeHistoryCursor>> {
        if let Some(cursor) = &request.after {
            cursor.validate(self.db_id, request).map_err(invalid)?;
        }
        let request = request.clone();
        self.episode_read(move |tx, db| history(tx, db, &request))
            .await
    }

    pub(super) async fn episode_references_impl(
        &self,
        request: &EpisodeReferencesRequest,
    ) -> Result<EpisodeReferencesPage> {
        if let Some(cursor) = &request.after {
            cursor.validate(self.db_id, request).map_err(invalid)?;
        }
        let request = request.clone();
        self.episode_read(move |tx, db| references(tx, db, &request))
            .await
    }
}

fn run(
    tx: &mut BoundedReadTransaction,
    script: &str,
    params: BTreeMap<String, DataValue>,
) -> Result<NamedRows> {
    tx.run_script(script, params).map_err(backend)
}

fn ensure_read_generation(tx: &mut BoundedReadTransaction, db_id: Ulid) -> Result<()> {
    // Check identity even when a recognized predecessor has no episode lane.
    // Capability fallback must not hide corruption or a changed owner binding.
    let rows = run(tx, "?[v] := *meta{k: 'db_id', v}", BTreeMap::new())?;
    if rows.rows.len() != 1 || want_str(&rows.rows[0][0])? != db_id.to_string() {
        return Err(backend_str("episode read database identity changed".into()));
    }
    let params = BTreeMap::from([("key".into(), dv_str(VECTOR_PROJECTION_META_KEY))]);
    let rows = run(tx, "?[v] := *meta{k: $key, v}", params)?;
    if rows.rows.len() != 1 {
        return Err(backend_str(
            "episode read generation marker missing or ambiguous".into(),
        ));
    }
    match want_str(&rows.rows[0][0])? {
        SINGLE_GRAPH_V1_CATALOG_GENERATION_MARKER
        | CONCERN_V1_CATALOG_GENERATION_MARKER
        | EPISODE_CONTEXT_V2_CATALOG_GENERATION_MARKER
        | TOUCHSTONES_V1_CATALOG_GENERATION_MARKER => Ok(()),
        CAPTURE_V1_CATALOG_GENERATION_MARKER
        | LEGACY_CATALOG_GENERATION_MARKER
        | MANAGED_WRITER_GENERATION_MARKER => Err(Error::EpisodeUnavailable(
            mneme_core::episode::EpisodeUnavailableReason::StoreNotUpgraded,
        )),
        _ => Err(backend_str(
            "episode read generation is unrecognized".into(),
        )),
    }
}

fn read_node(tx: &mut BoundedReadTransaction, id: NodeId) -> Result<Option<Node>> {
    let rows = run(
        tx,
        "?[id, data, status] := id = $id, *node{id, data, status}",
        BTreeMap::from([("id".into(), dv_str(&id.0.to_string()))]),
    )?;
    rows.rows
        .first()
        .map(|row| decode_canonical_node_row(row))
        .transpose()
}

fn read_head(tx: &mut BoundedReadTransaction, root: EpisodeId) -> Result<Option<NodeId>> {
    let rows = run(
        tx,
        "?[head] := *episode_head{root: $root, head}",
        BTreeMap::from([("root".into(), dv_str(&root.get().0.to_string()))]),
    )?;
    rows.rows
        .first()
        .map(|row| node_id(want_str(&row[0])?))
        .transpose()
}

fn current_header(
    tx: &mut BoundedReadTransaction,
    root: EpisodeId,
    edition: NodeId,
) -> Result<EpisodeHeader> {
    let current = read_head(tx, root)?
        .ok_or_else(|| backend_str("episode projection has no current head".into()))?;
    if current != edition {
        return Err(backend_str(
            "episode projection names a stale edition".into(),
        ));
    }
    let node = read_node(tx, edition)?
        .ok_or_else(|| backend_str("episode projection names a missing edition".into()))?;
    if identity(&node)?.episode_id != root {
        return Err(backend_str(
            "episode projection root disagrees with its edition".into(),
        ));
    }
    header(&node, current)
}

fn stored_time(value: &DataValue) -> Result<EpisodeTime> {
    EpisodeTime::new(graph_records::decode_storage_timestamp(
        value,
        "episode time",
    )?)
    .map_err(invalid)
}

fn optional_time(value: &DataValue) -> Result<Option<EpisodeTime>> {
    if matches!(value, DataValue::Null) {
        Ok(None)
    } else {
        stored_time(value).map(Some)
    }
}

fn occurrence(start: &DataValue, end: &DataValue) -> Result<OccurrenceSpan> {
    match (optional_time(start)?, optional_time(end)?) {
        (None, None) => Ok(OccurrenceSpan::Unknown),
        (Some(start), Some(end)) if start == end => Ok(OccurrenceSpan::Point { at: start }),
        (Some(start), Some(end)) if start < end => Ok(OccurrenceSpan::Range { start, end }),
        _ => Err(backend_str(
            "malformed episode occurrence projection".into(),
        )),
    }
}

fn matches_occurrence(span: &OccurrenceSpan, filter: &EpisodeOccurrenceFilter) -> bool {
    match filter {
        EpisodeOccurrenceFilter::Any => true,
        EpisodeOccurrenceFilter::Unknown => matches!(span, OccurrenceSpan::Unknown),
        EpisodeOccurrenceFilter::Overlaps(window) => span.overlaps(window),
    }
}

fn scan(
    tx: &mut BoundedReadTransaction,
    relation: &str,
    request: PrimaryKeyScan,
) -> Result<Vec<Vec<DataValue>>> {
    let limit = request.limit;
    let rows = tx
        .scan_relation_by_primary_key(relation, request)
        .map_err(backend)?
        .rows;
    if rows.len() > limit {
        return Err(backend_str(format!(
            "episode {relation} scan exceeded its physical row limit"
        )));
    }
    Ok(rows)
}

fn timeline(
    tx: &mut BoundedReadTransaction,
    db: Ulid,
    request: &EpisodeTimelineRequest,
) -> Result<EpisodePage<EpisodeTimelineCursor>> {
    let limit = request.limit.get();
    let axis = match request.axis {
        EpisodeTimelineAxis::Recorded => "recorded",
        EpisodeTimelineAxis::Occurred => "occurred",
    };
    let thread = request
        .filter
        .thread
        .as_ref()
        .map_or("", EpisodeThread::as_str);
    let descending = matches!(request.order, EpisodeOrder::NewestFirst);
    let mut position = request.after.as_ref().map(EpisodeTimelineCursor::key);
    let mut items = Vec::with_capacity(limit + 1);
    let mut emitted_keys = Vec::with_capacity(limit);
    let mut inspected = 0;
    loop {
        // Occurrence windows include ranges whose start precedes `from`.
        // Only the upper bound can narrow that physical start-time range.
        let lower_time = if matches!(request.axis, EpisodeTimelineAxis::Recorded) {
            request.window.as_ref().and_then(|window| window.from)
        } else {
            None
        };
        let upper_time = request.window.as_ref().and_then(|window| window.through);
        let mut lower = lower_time.map_or(PrimaryKeyScanBound::Unbounded, |time| {
            PrimaryKeyScanBound::Included(vec![dv_int(time.get() as i64), dv_str("")])
        });
        let mut upper = upper_time.map_or(PrimaryKeyScanBound::Unbounded, |time| {
            PrimaryKeyScanBound::Included(vec![dv_int(time.get() as i64), dv_str("~")])
        });
        if let Some((time, root)) = position {
            let key = vec![dv_int(time.get() as i64), dv_str(&root.get().0.to_string())];
            if descending {
                if upper_time.is_none_or(|through| time <= through) {
                    upper = PrimaryKeyScanBound::Excluded(key);
                }
            } else if lower_time.is_none_or(|from| time >= from) {
                lower = PrimaryKeyScanBound::Excluded(key);
            }
        }
        let take = (limit + 1 - items.len()).min(TIMELINE_SCAN_ROWS - inspected);
        let rows = scan(
            tx,
            "episode_time",
            PrimaryKeyScan {
                prefix: vec![dv_str(axis), dv_str(thread)],
                lower,
                upper,
                direction: if descending {
                    PrimaryKeyScanDirection::Descending
                } else {
                    PrimaryKeyScanDirection::Ascending
                },
                limit: take,
            },
        )?;
        let exhausted = rows.len() < take;
        for row in rows {
            if row.len() != 7 || want_str(&row[0])? != axis || want_str(&row[1])? != thread {
                return Err(backend_str(
                    "episode timeline index row escaped its key prefix".into(),
                ));
            }
            inspected += 1;
            let time = stored_time(&row[2])?;
            let root = EpisodeId::new(node_id(want_str(&row[3])?)?);
            position = Some((time, root));
            let occurred = occurrence(&row[5], &row[6])?;
            if !matches_occurrence(&occurred, &request.filter.occurrence)
                || matches!(request.axis, EpisodeTimelineAxis::Occurred)
                    && request
                        .window
                        .as_ref()
                        .is_some_and(|window| !occurred.overlaps(window))
            {
                continue;
            }
            let edition = node_id(want_str(&row[4])?)?;
            let item = current_header(tx, root, edition)?;
            if item.occurred != occurred
                || (!thread.is_empty()
                    && item.thread.as_ref().map(EpisodeThread::as_str) != Some(thread))
                || match request.axis {
                    EpisodeTimelineAxis::Recorded => item.recorded_at != time,
                    EpisodeTimelineAxis::Occurred => match item.occurred {
                        OccurrenceSpan::Unknown => true,
                        OccurrenceSpan::Point { at } => at != time,
                        OccurrenceSpan::Range { start, .. } => start != time,
                    },
                }
            {
                return Err(backend_str(
                    "episode timeline projection disagrees with canonical edition".into(),
                ));
            }
            if items.len() == limit {
                let (time, root) = *emitted_keys.last().expect("nonzero page limit");
                return Ok(EpisodePage {
                    items,
                    partial: false,
                    next: Some(EpisodeTimelineCursor::new(db, request, time, root)),
                });
            }
            items.push(item);
            emitted_keys.push((time, root));
        }
        if exhausted {
            return Ok(EpisodePage {
                items,
                partial: false,
                next: None,
            });
        }
        if inspected == TIMELINE_SCAN_ROWS {
            let (time, root) = position.expect("nonzero scan budget");
            return Ok(EpisodePage {
                items,
                partial: true,
                next: Some(EpisodeTimelineCursor::new(db, request, time, root)),
            });
        }
    }
}

fn cue(tx: &mut BoundedReadTransaction, request: &EpisodeCueRequest) -> Result<EpisodeCuePage> {
    let terms: BTreeSet<_> = crate::lexical_terms(request.cue.as_str())
        .into_iter()
        .collect();
    if terms.is_empty() {
        return Ok(EpisodeCuePage {
            mode: EpisodeCueMode::Lexical,
            items: Vec::new(),
            has_more: false,
            partial: false,
        });
    }
    let query = terms
        .into_iter()
        .map(|term| serde_json::to_string(&term).expect("FTS token serialization"))
        .collect::<Vec<_>>()
        .join(" OR ");
    let mut params = BTreeMap::from([
        ("query".into(), dv_str(&query)),
        ("k".into(), dv_int((request.limit.get() + 1) as i64)),
    ]);
    let mut filters = Vec::new();
    if let Some(thread) = &request.filter.thread {
        params.insert("thread".into(), dv_str(thread.as_str()));
        filters.push("thread == $thread");
    }
    match &request.filter.occurrence {
        EpisodeOccurrenceFilter::Any => (),
        EpisodeOccurrenceFilter::Unknown => filters.push("is_null(occurred_start)"),
        EpisodeOccurrenceFilter::Overlaps(window) => {
            // Cozo boolean conjunction is eager; nullable bounds need lazy
            // conditionals before their integer comparisons.
            filters.push("!is_null(occurred_start)");
            if let Some(from) = window.from {
                params.insert("from".into(), dv_int(from.get() as i64));
                filters.push("if(is_null(occurred_end), false, occurred_end >= $from)");
            }
            if let Some(through) = window.through {
                params.insert("through".into(), dv_int(through.get() as i64));
                filters.push("if(is_null(occurred_start), false, occurred_start <= $through)");
            }
        }
    }
    let filter = if filters.is_empty() {
        "".into()
    } else {
        format!(", filter: {}", filters.join(" && "))
    };
    // FTS applies its filter before selecting k. Its posting-list work depends
    // on term frequency; this is a bounded result, not a hard physical work cap.
    let script = format!(
        "?[root, head, score] := ~episode_search:fts{{root, head, thread, occurred_start, occurred_end | query: $query, k: $k, bind_score: score{filter}}} :order -score, root :limit $k"
    );
    let rows = run(tx, &script, params)?;
    let has_more = rows.rows.len() > request.limit.get();
    let mut items = Vec::with_capacity(request.limit.get());
    for row in rows.rows.iter().take(request.limit.get()) {
        let root = EpisodeId::new(node_id(want_str(&row[0])?)?);
        let item = current_header(tx, root, node_id(want_str(&row[1])?)?)?;
        if request
            .filter
            .thread
            .as_ref()
            .is_some_and(|thread| item.thread.as_ref() != Some(thread))
            || !matches_occurrence(&item.occurred, &request.filter.occurrence)
        {
            return Err(backend_str(
                "episode cue projection disagrees with canonical edition".into(),
            ));
        }
        items.push(item);
    }
    Ok(EpisodeCuePage {
        mode: EpisodeCueMode::Lexical,
        items,
        has_more,
        partial: has_more,
    })
}

fn history(
    tx: &mut BoundedReadTransaction,
    db: Ulid,
    request: &EpisodeHistoryRequest,
) -> Result<EpisodePage<EpisodeHistoryCursor>> {
    let Some(current) = read_head(tx, request.episode_id)? else {
        return Ok(EpisodePage {
            items: Vec::new(),
            next: None,
            partial: false,
        });
    };
    let rows = scan(
        tx,
        "episode_history",
        PrimaryKeyScan {
            prefix: vec![dv_str(&request.episode_id.get().0.to_string())],
            lower: request
                .after
                .as_ref()
                .map_or(PrimaryKeyScanBound::Unbounded, |cursor| {
                    PrimaryKeyScanBound::Excluded(vec![dv_int(cursor.key().0.get() as i64)])
                }),
            upper: PrimaryKeyScanBound::Unbounded,
            direction: PrimaryKeyScanDirection::Ascending,
            limit: request.limit.get() + 1,
        },
    )?;
    let has_more = rows.len() > request.limit.get();
    let mut items = Vec::with_capacity(request.limit.get());
    for row in rows.iter().take(request.limit.get()) {
        if row.len() != 3 || want_str(&row[0])? != request.episode_id.get().0.to_string() {
            return Err(backend_str("episode history index escaped its root".into()));
        }
        let ordinal = graph_records::decode_storage_u32(&row[1], "episode revision")?;
        let edition = node_id(want_str(&row[2])?)?;
        let node = read_node(tx, edition)?
            .ok_or_else(|| backend_str("episode history names a missing edition".into()))?;
        let item = header(&node, current)?;
        if item.identity.episode_id != request.episode_id || item.identity.revision.get() != ordinal
        {
            return Err(backend_str(
                "episode history projection disagrees with canonical edition".into(),
            ));
        }
        items.push(item);
    }
    let next = if has_more {
        let last = items.last().expect("nonzero page limit");
        Some(EpisodeHistoryCursor::new(
            db,
            request,
            last.identity.revision,
            last.identity.edition_id,
        ))
    } else {
        None
    };
    Ok(EpisodePage {
        items,
        next,
        partial: false,
    })
}

fn references(
    tx: &mut BoundedReadTransaction,
    db: Ulid,
    request: &EpisodeReferencesRequest,
) -> Result<EpisodeReferencesPage> {
    let params = BTreeMap::from([
        ("anchor".into(), dv_str(&request.anchor.0.to_string())),
        ("cap".into(), dv_int((MAX_INCIDENT_EDGES + 1) as i64)),
    ]);
    // Both legs start at an endpoint index. The store's incident-edge invariant
    // bounds this union, including weak/archived/non-traversable evidence.
    let rows = run(
        tx,
        "incident[from, to] := *edge{from: $anchor, to}, from = $anchor\n\
        incident[from, to] := *edge:by_to{to: $anchor, from}, to = $anchor\n\
        ?[from, to, weight, kind, last_reinforced, trials, interference] := incident[from, to],\
        *edge{from, to, weight, kind, last_reinforced, trials, interference} :limit $cap",
        params,
    )?;
    if rows.rows.len() > MAX_INCIDENT_EDGES {
        return Err(backend_str(
            "episode reference anchor exceeds the incident-edge storage bound".into(),
        ));
    }
    let mut edges = BTreeMap::new();
    for row in rows.rows {
        let from = node_id(want_str(&row[0])?)?;
        let to = node_id(want_str(&row[1])?)?;
        if request
            .after
            .as_ref()
            .is_some_and(|cursor| (from, to) <= cursor.key())
        {
            continue;
        }
        edges.insert((from, to), row_to_edge(from, to, &row[2..])?);
    }
    let ids: Vec<_> = edges
        .keys()
        .flat_map(|&(from, to)| [from, to])
        .collect::<BTreeSet<_>>()
        .into_iter()
        .collect();
    let mut nodes = BTreeMap::<NodeId, Node>::new();
    for ids in ids.chunks(MAX_NODE_HYDRATION_BATCH) {
        let params = BTreeMap::from([(
            "ids".into(),
            DataValue::List(ids.iter().map(|id| dv_str(&id.0.to_string())).collect()),
        )]);
        let rows = run(
            tx,
            "?[id, data, status] := id in $ids, *node{id, data, status}",
            params,
        )?;
        for row in rows.rows {
            let node = decode_canonical_node_row(&row)?;
            nodes.insert(node.id(), node);
        }
        if ids.iter().any(|id| !nodes.contains_key(id)) {
            return Err(backend_str(
                "episode reference has a missing endpoint".into(),
            ));
        }
    }
    let mut items = Vec::with_capacity(request.limit.get());
    let mut has_more = false;
    for ((from, to), mut edge) in edges {
        let from_episode = nodes[&from]
            .episode()
            .map(|_| identity(&nodes[&from]))
            .transpose()?;
        let to_episode = nodes[&to]
            .episode()
            .map(|_| identity(&nodes[&to]))
            .transpose()?;
        if from_episode.is_none() && to_episode.is_none() {
            continue;
        }
        if items.len() == request.limit.get() {
            has_more = true;
            break;
        }
        let rows = run(
            tx,
            "?[start, end] := *edge_anchor{from: $from, to: $to, start, end}",
            BTreeMap::from([
                ("from".into(), dv_str(&from.0.to_string())),
                ("to".into(), dv_str(&to.0.to_string())),
            ]),
        )?;
        edge.anchor = rows
            .rows
            .first()
            .map(|row| graph_records::decode_body_span(&row[0], &row[1]))
            .transpose()?;
        items.push(EpisodeReference {
            edge,
            from_episode,
            to_episode,
        });
    }
    let next = if has_more {
        let last = items.last().expect("nonzero page limit");
        Some(EpisodeReferencesCursor::new(
            db,
            request,
            last.edge.from,
            last.edge.to,
        ))
    } else {
        None
    };
    Ok(EpisodeReferencesPage { items, next })
}

#[cfg(test)]
mod availability_tests {
    use super::*;
    use mneme_core::episode::EpisodeUnavailableReason;

    fn cue() -> EpisodeCueRequest {
        EpisodeCueRequest {
            cue: EpisodeCue::new("lantern").unwrap(),
            filter: EpisodeFilter::default(),
            limit: EpisodePageLimit::default(),
        }
    }

    #[tokio::test]
    async fn fresh_context_generation_retains_episode_read_lane() {
        let store = CozoStore::new(4).unwrap();
        assert_eq!(
            store
                .read_meta(VECTOR_PROJECTION_META_KEY)
                .unwrap()
                .as_deref(),
            Some(TOUCHSTONES_V1_CATALOG_GENERATION_MARKER)
        );
        assert!(store.episode_cue(&cue()).await.unwrap().items.is_empty());
    }

    #[tokio::test]
    async fn recognized_predecessors_report_typed_episode_unavailability() {
        for marker in [
            LEGACY_CATALOG_GENERATION_MARKER,
            CAPTURE_V1_CATALOG_GENERATION_MARKER,
            MANAGED_WRITER_GENERATION_MARKER,
        ] {
            let store = CozoStore::new(4).unwrap();
            let original_db_id = store.db_id;
            store.put_meta(VECTOR_PROJECTION_META_KEY, marker).unwrap();
            assert!(matches!(
                store.episode_cue(&cue()).await,
                Err(Error::EpisodeUnavailable(
                    EpisodeUnavailableReason::StoreNotUpgraded
                ))
            ));
            assert_eq!(
                store.read_meta("db_id").unwrap(),
                Some(original_db_id.to_string())
            );
            assert_eq!(
                store
                    .read_meta(VECTOR_PROJECTION_META_KEY)
                    .unwrap()
                    .as_deref(),
                Some(marker)
            );
        }
    }

    #[tokio::test]
    async fn malformed_or_torn_episode_state_is_not_capability_fallback() {
        // Unknown markers do not mean a legitimate pre-episode store.
        let unknown = CozoStore::new(4).unwrap();
        unknown
            .put_meta(VECTOR_PROJECTION_META_KEY, "unrecognized-generation")
            .unwrap();
        assert!(matches!(
            unknown.episode_cue(&cue()).await,
            Err(Error::Backend(_))
        ));

        // Nor does a missing marker. Do not synthesize an empty episode lane.
        let missing = CozoStore::new(4).unwrap();
        missing
            .run(
                "?[k] <- [[$key]] :rm meta {k}",
                BTreeMap::from([("key".into(), dv_str(VECTOR_PROJECTION_META_KEY))]),
                true,
            )
            .unwrap();
        assert!(matches!(
            missing.episode_cue(&cue()).await,
            Err(Error::Backend(_))
        ));

        // A current marker with absent episode projections is a real read failure.
        let torn = CozoStore::new(4).unwrap();
        torn.run("::index drop episode_search:fts", BTreeMap::new(), true)
            .unwrap();
        torn.run("::remove episode_search", BTreeMap::new(), true)
            .unwrap();
        assert!(matches!(
            torn.episode_cue(&cue()).await,
            Err(Error::Backend(_))
        ));
    }

    #[tokio::test]
    async fn changed_database_identity_precedes_legacy_capability_fallback() {
        let store = CozoStore::new(4).unwrap();
        store
            .put_meta(
                VECTOR_PROJECTION_META_KEY,
                CAPTURE_V1_CATALOG_GENERATION_MARKER,
            )
            .unwrap();
        let other = Ulid::new();
        assert_ne!(other, store.db_id);
        store.put_meta("db_id", &other.to_string()).unwrap();
        assert!(matches!(
            store.episode_cue(&cue()).await,
            Err(Error::Backend(_))
        ));
        assert_eq!(store.read_meta("db_id").unwrap(), Some(other.to_string()));
    }
}
