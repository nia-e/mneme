use super::{Result, digest, fixture, require};
use fixture::Fixture;
use mneme_core::NodeId;
use mneme_core::ports::{ColdPath, Embedder, LexicalIndex, StatusFilter, VectorIndex};
use mneme_cozo::MemStore;
use mneme_embed::HashingEmbedder;
use serde_json::{Value, json};
use std::collections::BTreeSet;
use std::sync::Arc;

async fn pages(f: &Fixture, mut request: Value) -> Result<Vec<Value>> {
    let mut rows = Vec::new();
    let mut cursors = BTreeSet::new();
    for _ in 0..16 {
        let response = f.episode(request.clone()).await?;
        require(
            response["partial"] == false,
            "fixture page unexpectedly partial",
        )?;
        let items = response["items"].as_array().ok_or("page has no items")?;
        require(
            items.len() <= request["limit"].as_u64().unwrap_or(8) as usize,
            "page exceeds item bound",
        )?;
        rows.extend(items.iter().cloned());
        if response["next"].is_null() {
            return Ok(rows);
        }
        let cursor = response["next"].as_str().ok_or("invalid page cursor")?;
        require(
            cursors.insert(cursor.to_string()),
            "pagination repeated a cursor",
        )?;
        request["after"] = json!(cursor);
    }
    Err("fixture page loop exceeded finite 16-page envelope".into())
}

fn row_ids(rows: &[Value], field: &str) -> Result<Vec<NodeId>> {
    rows.iter()
        .map(|row| fixture::parse_id(&row[field]))
        .collect()
}

fn aliases(f: &Fixture, array: &Value) -> Result<Vec<NodeId>> {
    array
        .as_array()
        .ok_or("alias array missing")?
        .iter()
        .map(|alias| f.id(alias.as_str().ok_or("alias must be text")?))
        .collect()
}

/// InlineStore intentionally mints body URIs. Keep the actual export for round-trip
/// checks, but replace those physical names with content digests in repeatability
/// artifacts. Sorting removes adapter iteration order, never content or evidence.
async fn canonical(f: &Fixture, episode_only: bool) -> Result<Value> {
    let export = f.store.export();
    let episode_ids: BTreeSet<_> = export
        .nodes
        .iter()
        .filter(|n| !n.is_semantic())
        .map(|n| n.id())
        .collect();
    let mut nodes = Vec::new();
    for node in &export.nodes {
        if episode_only && node.is_semantic() {
            continue;
        }
        let mut value = serde_json::to_value(node)?;
        value["body"] = json!({"content_sha256":digest(&f.memory.resolve_body(node).await?)});
        nodes.push(value);
    }
    nodes.sort_by_key(|node| node["id"].to_string());
    let mut edges: Vec<Value> = export
        .edges
        .iter()
        .filter(|edge| {
            !episode_only || episode_ids.contains(&edge.from) || episode_ids.contains(&edge.to)
        })
        .map(serde_json::to_value)
        .collect::<std::result::Result<_, _>>()?;
    edges.sort_by_key(Value::to_string);
    Ok(json!({"nodes":nodes,"edges":edges}))
}

pub async fn run() -> Result<Value> {
    let f = Fixture::load().await?;
    let corpus = fixture::corpus();
    let assessment = fixture::assessment();
    let expected = &assessment["mechanics"];
    let mut checks = Vec::new();
    let snapshot = f.store.export();
    require(
        snapshot.nodes.len() == 12,
        "expected 9 episode editions and 3 semantic nodes",
    )?;
    require(
        snapshot.nodes.iter().filter(|n| !n.is_semantic()).count() == 9,
        "typed edition count mismatch",
    )?;
    for lane in ["episodes", "semantic_notes", "editorial_revisions"] {
        for record in corpus[lane].as_array().unwrap() {
            let id = f.id(record["alias"].as_str().unwrap())?;
            let node = f
                .memory
                .get_node(id)
                .await?
                .ok_or("authored node missing")?;
            require(
                node.summary() == record["summary"].as_str().unwrap(),
                "summary changed",
            )?;
            require(
                f.memory.resolve_body(&node).await? == record["body"].as_str().unwrap().as_bytes(),
                "body changed",
            )?;
        }
    }
    checks.push("typed_corpus_round_trip");

    let timeline = pages(
        &f,
        json!({"action":"list","axis":"occurred","order":"oldest_first","limit":2}),
    )
    .await?;
    let mut expected_occurrence = Vec::new();
    for group in expected["occurrence_ascending_groups"].as_array().unwrap() {
        let mut tied = aliases(&f, group)?;
        tied.sort(); // Public occurred order is (occurrence-start, stable root ID).
        expected_occurrence.extend(tied);
    }
    require(
        row_ids(&timeline, "episode_id")? == expected_occurrence,
        "occurrence order/tie/page mismatch",
    )?;
    let recording = pages(
        &f,
        json!({"action":"list","axis":"recorded","order":"oldest_first","limit":2}),
    )
    .await?;
    require(
        row_ids(&recording, "episode_id")? == aliases(&f, &expected["root_recording_ascending"])?,
        "recording chronology mismatch",
    )?;
    for (thread, roots) in expected["thread_roots"].as_object().unwrap() {
        let rows = pages(&f, json!({"action":"list","thread":thread,"limit":2})).await?;
        let actual: BTreeSet<_> = row_ids(&rows, "episode_id")?.into_iter().collect();
        require(
            actual == aliases(&f, roots)?.into_iter().collect(),
            "thread filtering mismatch",
        )?;
    }
    for row in &timeline {
        let root = fixture::parse_id(&row["episode_id"])?;
        let alias = f
            .aliases
            .iter()
            .find(|(_, id)| **id == root)
            .ok_or("root alias missing")?
            .0;
        let edition = expected["current_editions"][alias]
            .as_str()
            .ok_or("current edition expectation missing")?;
        require(
            fixture::parse_id(&row["edition_id"])? == f.id(edition)?,
            "timeline returned an old edition",
        )?;
    }
    checks.push("bounded_timeline_pages_ties_late_recording_threads");

    let root = f.id("e05")?;
    let current = f.id("e05-r1")?;
    let detail = f
        .episode(json!({"action":"get","episode_id":root,"body":true}))
        .await?;
    let original = f
        .episode(json!({"action":"get","episode_id":root,"edition_id":root,"body":true}))
        .await?;
    require(
        detail["edition_id"] == json!(current) && detail["is_current"] == true,
        "root did not resolve current",
    )?;
    require(
        original["edition_id"] == json!(root) && original["is_current"] == false,
        "historical edition is not exact",
    )?;
    require(
        original["body"].as_str().unwrap().contains("lovely amreb"),
        "editorial change erased original",
    )?;
    require(
        detail["body"].as_str().unwrap().contains("lovely amber"),
        "current editorial body missing",
    )?;
    require(
        detail["recorded_at"] == original["recorded_at"]
            && detail["occurred"] == original["occurred"],
        "edit moved original event time",
    )?;
    require(
        detail["edition_recorded_at"].as_u64() > original["edition_recorded_at"].as_u64(),
        "edition clock did not advance",
    )?;
    let history = pages(&f, json!({"action":"history","episode_id":root,"limit":1})).await?;
    require(
        row_ids(&history, "edition_id")? == vec![root, current],
        "history order changed",
    )?;
    require(
        history[0]["revision"] == 0 && history[1]["revision"] == 1,
        "edition ordinal mismatch",
    )?;
    let exact = f.exact(root, 1024).await?;
    require(
        exact["body"] == original["body"],
        "generic exact read rewrote old edition",
    )?;
    checks.push("immutable_editions_root_resolution_and_history");

    for case in expected["cue_cases"].as_array().unwrap() {
        let result = f
            .episode(json!({"action":"search","cue":case["cue"],"limit":32}))
            .await?;
        require(
            result["mode"] == "lexical",
            "cue did not label lexical mode",
        )?;
        let roots: BTreeSet<_> = row_ids(result["items"].as_array().unwrap(), "episode_id")?
            .into_iter()
            .collect();
        require(
            aliases(&f, &case["required_roots"])?
                .into_iter()
                .all(|id| roots.contains(&id)),
            "natural cue missed required scene",
        )?;
    }
    let old_cue = f.episode(json!({"action":"search","cue":"amreb"})).await?;
    require(
        old_cue["items"].as_array().unwrap().is_empty(),
        "cue indexed an old edition",
    )?;
    let references = pages(&f, json!({"action":"references","anchor":root,"limit":1})).await?;
    require(
        references.iter().any(|r| {
            r["edge"]["from"] == json!(f.id("e08").unwrap()) && r["edge"]["to"] == json!(root)
        }),
        "incoming original reference disappeared",
    )?;
    let lesson_refs = pages(
        &f,
        json!({"action":"references","anchor":f.id("l03")?,"limit":1}),
    )
    .await?;
    require(
        lesson_refs.len() == 2,
        "lesson-to-experience references missing",
    )?;
    for episode in ["e02", "e07"] {
        let incoming = pages(
            &f,
            json!({"action":"references","anchor":f.id(episode)?,"limit":1}),
        )
        .await?;
        require(
            incoming
                .iter()
                .any(|r| r["edge"]["from"] == json!(f.id("l03").unwrap())),
            "reverse lesson reference missing",
        )?;
    }
    checks.push("lexical_current_summary_cues_and_bidirectional_references");

    let mut chunks = String::new();
    let mut offset = 0;
    for _ in 0..32 {
        let page = f.episode(json!({"action":"get","episode_id":root,"body":true,"offset":offset,"max_bytes":17})).await?;
        require(
            page["body"].as_str().unwrap().len() <= 17,
            "body byte bound exceeded",
        )?;
        chunks.push_str(page["body"].as_str().unwrap());
        match page["body_range"]["next_offset"].as_u64() {
            None => break,
            Some(next) => {
                require(next > offset, "body continuation did not advance")?;
                offset = next;
            }
        }
    }
    require(
        chunks == detail["body"].as_str().unwrap(),
        "bounded body reads did not reconstruct content",
    )?;
    let first = f.episode(json!({"action":"list","limit":1})).await?;
    require(
        first["next"].is_string(),
        "small page did not advertise continuation",
    )?;
    require(
        f.episode(json!({"action":"list","thread":"rainmap","limit":1,"after":first["next"]}))
            .await
            .is_err(),
        "cursor accepted changed filters",
    )?;
    require(
        f.episode(json!({"action":"list","limit":33}))
            .await
            .is_err(),
        "oversize page accepted",
    )?;
    checks.push("body_ranges_page_bounds_and_filter_bound_cursors");

    let before = canonical(&f, false).await?;
    for alias in ["e05", "e05-r1"] {
        let replay = f.episode(f.writes[alias].clone()).await?;
        require(
            replay["replayed"] == true && replay["edition_id"] == json!(f.id(alias)?),
            "source-key replay was not exact",
        )?;
    }
    let mut conflict = f.writes["e05"].clone();
    conflict["summary"] = json!("Changed content under the same source key");
    require(
        f.episode(conflict).await.is_err(),
        "conflicting append edited in place",
    )?;
    let mut stale = f.writes["e05-r1"].clone();
    stale["source"]["key"] = json!("episodic-v1/stale-competing-editorial-key");
    require(
        f.episode(stale).await.is_err(),
        "competing stale revision was accepted",
    )?;
    require(
        canonical(&f, false).await? == before,
        "replay/refusal changed canonical state",
    )?;
    checks.push("append_and_editorial_replay_conflict_without_mutation");

    let query = "Lantern relationships graph neighborhood reads";
    let vector = HashingEmbedder::new(fixture::DIMENSION)
        .embed_query(query)
        .await?;
    let ann = f.store.ann(&vector, 1, StatusFilter::default()).await?;
    let lexical = f.store.search(query, 1, StatusFilter::default()).await?;
    require(
        ann.len() == 1 && lexical.len() == 1,
        "episodes starved semantic top-one",
    )?;
    for hit in ann.iter().chain(&lexical) {
        require(
            f.memory
                .get_node(hit.id)
                .await?
                .ok_or("ranked node missing")?
                .is_semantic(),
            "episode entered semantic seed ranking",
        )?;
    }
    for text in [query, "kite", "missing records"] {
        let batch = f.memory.retrieve_batch(text).await?;
        for hit in batch.primary.iter().chain(&batch.probationary) {
            require(hit.node.is_semantic(), "episode entered semantic expansion")?;
        }
    }
    let old_lesson = f.memory.get_node(f.id("l01")?).await?.unwrap();
    require(
        !old_lesson.is_active(),
        "old lesson remains active after supersession",
    )?;
    require(
        f.memory.get_node(f.id("l02")?).await?.unwrap().is_active(),
        "current lesson is not active",
    )?;
    checks.push("semantic_seed_budget_expansion_and_supersession_separation");

    let serialized = serde_json::to_vec(&f.store.export())?;
    let imported = Arc::new(MemStore::from_export(serde_json::from_slice(&serialized)?)?);
    let mut before_export: Value = serde_json::from_slice(&serialized)?;
    let mut after_export: Value = serde_json::from_slice(&serde_json::to_vec(&imported.export())?)?;
    for value in [&mut before_export, &mut after_export] {
        for field in value.as_object_mut().unwrap().values_mut() {
            if let Some(items) = field.as_array_mut() {
                items.sort_by_key(Value::to_string);
            }
        }
    }
    require(
        before_export == after_export,
        "export/import lost canonical fields, vectors or source proofs",
    )?;
    let restored = Fixture {
        memory: fixture::memory(imported.clone(), f.bodies.clone(), f.clock.clone()),
        store: imported,
        bodies: f.bodies.clone(),
        clock: f.clock.clone(),
        aliases: f.aliases.clone(),
        writes: f.writes.clone(),
        trace: Vec::new(),
    };
    require(
        canonical(&restored, false).await? == before,
        "canonical export/import lost content or evidence",
    )?;
    require(
        pages(
            &restored,
            json!({"action":"list","axis":"occurred","order":"oldest_first","limit":2}),
        )
        .await?
            == timeline,
        "import did not rebuild current/time projections",
    )?;
    require(
        pages(
            &restored,
            json!({"action":"history","episode_id":root,"limit":1}),
        )
        .await?
            == history,
        "import lost editorial history",
    )?;
    checks.push("canonical_export_import_and_index_reconstruction_shared_inline_bodies");

    let preserved = canonical(&restored, true).await?;
    let mut config = fixture::config();
    config.prune_weight_floor = 1.0;
    config.dense_degree_threshold = 1;
    config.bridge_probability = 1.0;
    let maintenance = fixture::memory_with_config(
        restored.store.clone(),
        restored.bodies.clone(),
        restored.clock.clone(),
        config,
    );
    maintenance.decay_sweep(ColdPath::acquire()).await?;
    maintenance.prune_dense(ColdPath::acquire()).await?;
    maintenance.promote_candidates(ColdPath::acquire()).await?;
    maintenance
        .consolidate(
            ColdPath::acquire(),
            &restored.aliases.values().copied().collect::<Vec<_>>(),
        )
        .await?;
    require(
        canonical(&restored, true).await? == preserved,
        "semantic maintenance changed experience or incident evidence",
    )?;
    checks.push("aggressive_semantic_maintenance_preserves_episode_nodes_and_edges");

    let mut result = json!({
        "schema":"mneme.episodic-v1.rehearsal.v1","actor_kind":"scripted_rehearsal",
        "fixture_sha256":fixture::fingerprint(),"checks":checks,"all_passed":true,
        "authorship":f.trace,"aliases":f.aliases,"canonical_content":before,
        "occurred_timeline":timeline,"recorded_timeline":recording,"editorial_history":history,
        "claims":{"fresh_actor_evidence":false,"natural_authoring_evidence":false,"graph_advantage":false,
                  "provider_tokens":null,"provider_cost":null,"runtime":"isolated MemStore, InlineStore and hashing embeddings",
                  "export_scope":"canonical round-trip with shared in-memory body resolver; not a packaged backup or persistent migration"}
    });
    result["semantic_digest"] = json!(digest(&serde_json::to_vec(&result)?));
    Ok(result)
}
