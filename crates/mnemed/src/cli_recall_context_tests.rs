use super::*;

fn context_args(extra: &[&str]) -> RecallContextArgs {
    let mut argv = vec!["mnemed", "recall-context", "bounded recall"];
    argv.extend_from_slice(extra);
    let cli = Cli::try_parse_from(argv).unwrap();
    let Command::RecallContext(args) = cli.command else {
        panic!("expected recall-context command");
    };
    args
}

#[test]
fn context_budgets_follow_request_with_fixed_bytes_and_episode_policy() {
    let (retrieval, k, presentation) =
        validate_cli_recall_context(&context_args(&[]), Budget::default(), 5).unwrap();
    assert_eq!(k, 5);
    assert_eq!(retrieval.max_nodes, Budget::default().max_nodes);
    assert_eq!(presentation.max_content_bytes(), DEFAULT_CLI_CONTEXT_BYTES);
    assert_eq!(
        presentation.control_reserve_bytes(),
        PresentationBudget::minimum_control_reserve_bytes()
    );
    assert_eq!(presentation.max_items(), retrieval.max_nodes as u16 * 2);
    assert_eq!(
        presentation.lanes().primary().max_items(),
        retrieval.max_nodes as u16
    );
    let (_, _, larger) =
        validate_cli_recall_context(&context_args(&["--max-nodes", "24"]), Budget::default(), 5)
            .unwrap();
    assert_eq!(larger.lanes().primary().max_items(), 24);
    assert_eq!(larger.max_items(), 48);
    assert_eq!(
        presentation.max_summary_bytes_each(),
        MAX_CLI_CONTEXT_SUMMARY_BYTES
    );
    assert_eq!(presentation.body(), BodyBudget::disabled());
    assert_eq!(presentation.lanes().primary().hard_min_items(), 1);
    assert_eq!(presentation.lanes().core().max_items(), 0);
    assert_eq!(presentation.lanes().expansion().max_items(), 0);
    assert_eq!(presentation.lanes().episodic().target_items(), 2);
    assert_eq!(
        presentation.lanes().episodic().max_items(),
        retrieval.max_nodes as u16
    );

    let smallest = cli_presentation_budget(None, 1).unwrap();
    assert_eq!(smallest.max_items(), 2);
    assert_eq!(smallest.lanes().episodic().max_items(), 1);
    assert_eq!(smallest.lanes().episodic().target_items(), 1);
    assert_eq!(smallest.max_content_bytes(), DEFAULT_CLI_CONTEXT_BYTES);

    let minimum = context_args(&["--max-content-bytes", "4096"]);
    assert_eq!(
        validate_cli_recall_context(&minimum, Budget::default(), 5)
            .unwrap()
            .2
            .max_content_bytes(),
        MIN_CLI_CONTEXT_BYTES
    );
    for bytes in ["4095", "32769"] {
        assert!(
            validate_cli_recall_context(
                &context_args(&["--max-content-bytes", bytes]),
                Budget::default(),
                5,
            )
            .is_err()
        );
    }
    assert!(Cli::try_parse_from(["mnemed", "recall-context", "q", "--bodies"]).is_err());
    assert!(Cli::try_parse_from(["mnemed", "recall-context", "q", "--archived"]).is_err());
}

#[tokio::test]
async fn context_plan_composes_typed_retrieval_and_exact_packing_without_receipt() {
    let store = Arc::new(MemStore::new(DEFAULT_DIM));
    let mem = Memory::new(
        store.clone(),
        store.clone(),
        store,
        Arc::new(HashingEmbedder::new(DEFAULT_DIM)),
        Arc::new(SystemClock),
        Config {
            graph_seed_cap: 0,
            ..Config::default()
        },
    )
    .with_body_store(Arc::new(InlineStore::new()));
    let primary = mem
        .ingest(Ingest::new(
            "bounded recall primary",
            b"",
            &[],
            Provenance::derived_empty(),
        ))
        .await
        .unwrap();
    let probationary = mem
        .ingest(Ingest::new(
            "bounded recall candidate",
            b"",
            &[],
            Provenance::derived_empty(),
        ))
        .await
        .unwrap();

    let episode = mneme_app::episode::PreparedEpisode::parse(&serde_json::json!({
        "action": "append",
        "summary": "bounded recall incident",
        "body": "episode body must not be emitted",
        "source": {"namespace": "cli-context-test", "key": "incident", "reference": "test://context/episode"},
        "occurred": {"kind": "point", "at": 1000},
        "thread": "bounded-context"
    }))
    .unwrap()
    .run(&mem, ulid::Ulid::new(), None)
    .await
    .unwrap();

    let args = context_args(&[
        "--k",
        "2",
        "--depth",
        "0",
        "--max-nodes",
        "2",
        "--min-relevance",
        "0",
        "--max-content-bytes",
        "4096",
    ]);
    let plan = recall_context_plan(&mem, &args).await.unwrap();
    let value: Value = serde_json::from_str(plan.rendered_content()).unwrap();
    assert_eq!(value["schema"], "mneme.context.v7");
    assert_eq!(value["retrieval"]["mode"], "untagged");
    assert_eq!(value["retrieval"]["partial"], false);
    assert!(value["receipt"].is_null());
    assert_eq!(value["core"].as_array().unwrap().len(), 0);
    assert_eq!(value["expansions"].as_array().unwrap().len(), 0);
    assert_eq!(value["primary"].as_array().unwrap().len(), 2);
    assert!(value.get("probationary").is_none());
    assert_eq!(value["episodes"].as_array().unwrap().len(), 1);
    assert_eq!(value["episodic_retrieval"]["state"], "searched");
    assert_eq!(value["episodic_retrieval"]["mode"], "lexical");
    let episode_card = &value["episodes"][0];
    assert_eq!(episode_card["kind"], "episode");
    assert_eq!(episode_card["id"], episode["edition_id"]);
    assert_eq!(episode_card["episode_id"], episode["episode_id"]);
    assert_eq!(episode_card["edition_id"], episode["edition_id"]);
    assert_eq!(episode_card["current_edition_id"], episode["edition_id"]);
    assert_eq!(
        episode_card["occurred"],
        serde_json::json!({"kind": "point", "at": 1000})
    );
    assert_eq!(episode_card["thread"], "bounded-context");
    assert!(
        !plan
            .rendered_content()
            .contains("episode body must not be emitted")
    );
    assert!(
        plan.envelope()
            .primary()
            .iter()
            .any(|card| card.id() == primary)
    );
    assert!(
        plan.envelope()
            .primary()
            .iter()
            .any(|card| card.id() == probationary)
    );
    assert_eq!(plan.envelope().receipt(), None);
    assert_eq!(
        plan.rendered_content().len(),
        plan.envelope().usage().content_bytes() as usize
    );
    assert!(plan.rendered_content().len() <= 4096);
    assert_eq!(plan.manifest().cards().len(), 3);
    let episode_id: mneme_core::NodeId =
        serde_json::from_value(episode["edition_id"].clone()).unwrap();
    assert!(
        plan.manifest()
            .cards()
            .iter()
            .any(|card| card.node_id() == episode_id)
    );

    for id in [primary, probationary, episode_id] {
        let node = mem.get_node(id).await.unwrap().unwrap();
        assert_eq!(node.exposure_count(), 0);
        assert_eq!(node.last_exposed(), None);
    }
}

#[tokio::test]
async fn exact_tagged_query_and_context_share_one_primary_lane() {
    let store = Arc::new(MemStore::new(DEFAULT_DIM));
    let mem = Memory::new(
        store.clone(),
        store.clone(),
        store,
        Arc::new(HashingEmbedder::new(DEFAULT_DIM)),
        Arc::new(SystemClock),
        Config {
            graph_seed_cap: 0,
            ..Config::default()
        },
    )
    .with_body_store(Arc::new(InlineStore::new()));
    let tag = "é".repeat(mneme_core::MAX_TAG_BYTES / "é".len());
    assert_eq!(tag.len(), mneme_core::MAX_TAG_BYTES);
    mem.ingest(Ingest::new(
        "exact tagged active",
        b"",
        &[tag.as_str()],
        Provenance::derived_empty(),
    ))
    .await
    .unwrap();
    mem.ingest(Ingest::new(
        "exact tagged candidate",
        b"",
        &[tag.as_str()],
        Provenance::derived_empty(),
    ))
    .await
    .unwrap();

    let query_args = QueryArgs {
        text: "exact tagged".into(),
        k: Some(2),
        depth: Some(0),
        max_nodes: Some(2),
        min_relevance: Some(0.0),
        archived: false,
        tags: vec![tag.clone()],
        bodies: false,
        max_body_bytes: None,
    };
    let (batch, body_limit) = retrieve_cli_query(&mem, &query_args).await.unwrap();
    let query = cli_query_envelope(&mem, batch, false, body_limit)
        .await
        .unwrap();
    let query = serde_json::to_value(query).unwrap();
    assert_eq!(query["schema"], "mneme.query.v3");
    assert_eq!(query["mode"], "tagged");
    assert_eq!(query["partial"], false);
    assert_eq!(
        query["lanes"]["primary"]["seed_coverage"]["strategy"],
        "exact_cosine"
    );
    assert!(query["lanes"].get("probationary").is_none());
    assert_eq!(
        query["lanes"]["primary"]["hits"].as_array().unwrap().len(),
        2
    );
    assert_eq!(query["work"]["raw_memberships"], 2);
    assert_eq!(query["work"]["exact_hydrated_ids"], 2);
    assert_eq!(query["work"]["exact_vector_components"], 2 * DEFAULT_DIM);
    assert_eq!(
        query["stamp"]["projection_watermarks"][0]["projection"],
        "tag-membership"
    );
    let watermark = query["stamp"]["projection_watermarks"][0]["watermark"]
        .as_str()
        .unwrap()
        .to_owned();

    let context_args = RecallContextArgs {
        text: "exact tagged".into(),
        k: Some(2),
        depth: Some(0),
        max_nodes: Some(2),
        min_relevance: Some(0.0),
        tags: vec![tag],
        max_content_bytes: Some(4096),
    };
    let plan = recall_context_plan(&mem, &context_args).await.unwrap();
    let context: Value = serde_json::from_str(plan.rendered_content()).unwrap();
    assert_eq!(context["schema"], "mneme.context.v7");
    assert_eq!(context["retrieval"]["mode"], "tagged");
    assert_eq!(
        context["episodic_retrieval"]["state"],
        "not_searched_tag_filter"
    );
    assert_eq!(context["episodes"], serde_json::json!([]));
    assert_eq!(context["retrieval"]["partial"], false);
    assert_eq!(context["retrieval"]["work"], query["work"]);
    assert_eq!(
        context["retrieval"]["stamp"]["projection_watermarks"][0]["watermark"],
        watermark
    );
    assert_eq!(context["primary"].as_array().unwrap().len(), 2);
    assert!(context.get("probationary").is_none());
    assert_eq!(
        context["usage"]["content_bytes"].as_u64().unwrap() as usize,
        plan.rendered_content().len()
    );
}

#[tokio::test]
async fn popular_tag_partial_coverage_is_reported_without_a_reserved_slot() {
    const DIM: usize = 8;
    let store = Arc::new(MemStore::new(DIM));
    let config = Config {
        ann_k: 2,
        lexical_k: 0,
        graph_seed_cap: 0,
        graph_slot_cap: 0,
        similarity_link_cap: 0,
        min_similarity_links: 0,
        ..Config::default()
    };
    let mem = Memory::new(
        store.clone(),
        store.clone(),
        store,
        Arc::new(HashingEmbedder::new(DIM)),
        Arc::new(SystemClock),
        config,
    )
    .with_body_store(Arc::new(InlineStore::new()));
    let tag = "public-popular";
    for index in 0..mneme_core::tagged::MAX_TAGGED_RAW_MEMBERSHIPS {
        let summary = format!("public popular active {index}");
        mem.ingest(Ingest::new(
            &summary,
            b"",
            &[tag],
            Provenance::derived_empty(),
        ))
        .await
        .unwrap();
    }
    let _formerly_candidate = mem
        .ingest(Ingest::new(
            "public popular candidate",
            b"",
            &[tag],
            Provenance::derived_empty(),
        ))
        .await
        .unwrap();

    let query_args = QueryArgs {
        text: "public popular candidate".into(),
        k: Some(2),
        depth: Some(0),
        max_nodes: Some(2),
        min_relevance: Some(0.0),
        archived: false,
        tags: vec![tag.into()],
        bodies: false,
        max_body_bytes: None,
    };
    let (batch, body_limit) = retrieve_cli_query(&mem, &query_args).await.unwrap();
    let query = cli_query_envelope(&mem, batch, false, body_limit)
        .await
        .unwrap();
    let query = serde_json::to_value(query).unwrap();
    assert_eq!(query["schema"], "mneme.query.v3");
    assert_eq!(query["mode"], "tagged");
    assert_eq!(query["partial"], true);
    assert_eq!(
        query["work"]["raw_memberships"],
        mneme_core::tagged::MAX_TAGGED_RAW_MEMBERSHIPS + 1
    );
    assert_ne!(
        query["lanes"]["primary"]["seed_coverage"]["strategy"],
        "exact_cosine"
    );
    assert!(query["lanes"].get("probationary").is_none());
    let watermark = query["stamp"]["projection_watermarks"][0]["watermark"]
        .as_str()
        .unwrap()
        .to_owned();

    let context_args = RecallContextArgs {
        text: "public popular candidate".into(),
        k: Some(2),
        depth: Some(0),
        max_nodes: Some(2),
        min_relevance: Some(0.0),
        tags: vec![tag.into()],
        max_content_bytes: Some(4096),
    };
    let plan = recall_context_plan(&mem, &context_args).await.unwrap();
    let context: Value = serde_json::from_str(plan.rendered_content()).unwrap();
    assert_eq!(context["schema"], "mneme.context.v7");
    assert_eq!(context["retrieval"]["mode"], "tagged");
    assert_eq!(
        context["episodic_retrieval"]["state"],
        "not_searched_tag_filter"
    );
    assert_eq!(context["episodes"], serde_json::json!([]));
    assert_eq!(context["retrieval"]["partial"], true);
    assert_eq!(context["retrieval"]["work"], query["work"]);
    assert_eq!(
        context["retrieval"]["stamp"]["projection_watermarks"][0]["watermark"],
        watermark
    );
    assert!(context.get("probationary").is_none());
    assert_eq!(
        context["usage"]["content_bytes"].as_u64().unwrap() as usize,
        plan.rendered_content().len()
    );
}

#[tokio::test]
async fn untagged_graph_expansion_survives_the_public_cli_query_path() {
    const DIM: usize = 8;
    let store = Arc::new(MemStore::new(DIM));
    let mem = Memory::new(
        store.clone(),
        store.clone(),
        store,
        Arc::new(HashingEmbedder::new(DIM)),
        Arc::new(SystemClock),
        Config {
            ann_k: 1,
            lexical_k: 0,
            graph_seed_cap: 1,
            graph_slot_cap: 1,
            similarity_link_cap: 0,
            min_similarity_links: 0,
            ..Config::default()
        },
    )
    .with_body_store(Arc::new(InlineStore::new()));
    let root = mem
        .ingest(Ingest::new(
            "unique public graph root",
            b"",
            &[],
            Provenance::derived_empty(),
        ))
        .await
        .unwrap();
    let expansion = mem
        .ingest(Ingest::new(
            "otherwise unrelated expansion",
            b"",
            &[],
            Provenance::derived_empty(),
        ))
        .await
        .unwrap();
    mem.link(root, expansion, EdgeKind::Associative, 1.0, None)
        .await
        .unwrap();

    let args = QueryArgs {
        text: "unique public graph root".into(),
        k: Some(1),
        depth: Some(1),
        max_nodes: Some(2),
        min_relevance: Some(0.0),
        archived: false,
        tags: Vec::new(),
        bodies: false,
        max_body_bytes: None,
    };
    let (batch, body_limit) = retrieve_cli_query(&mem, &args).await.unwrap();
    let envelope = cli_query_envelope(&mem, batch, false, body_limit)
        .await
        .unwrap();
    let value = serde_json::to_value(envelope).unwrap();
    assert_eq!(value["mode"], "untagged");
    let hits = value["lanes"]["primary"]["hits"].as_array().unwrap();
    let expanded = hits
        .iter()
        .find(|hit| hit["id"] == serde_json::to_value(expansion).unwrap())
        .expect("linked node survives public query presentation");
    assert!(expanded["evidence"]["graph_rank"].as_u64().is_some());
}

#[tokio::test]
async fn cli_request_capacity_can_present_more_than_thirteen_short_cards() {
    let store = Arc::new(MemStore::new(DEFAULT_DIM));
    let mem = Memory::new(
        store.clone(),
        store.clone(),
        store,
        Arc::new(HashingEmbedder::new(DEFAULT_DIM)),
        Arc::new(SystemClock),
        Config {
            graph_seed_cap: 0,
            lexical_k: 0,
            ..Config::default()
        },
    )
    .with_body_store(Arc::new(InlineStore::new()));
    for n in 0..20 {
        mem.ingest(Ingest::new(
            &format!("bounded recall distinct lesson {n}"),
            b"",
            &[],
            Provenance::derived_empty(),
        ))
        .await
        .unwrap();
    }
    let plan = recall_context_plan(
        &mem,
        &context_args(&[
            "--k",
            "20",
            "--max-nodes",
            "20",
            "--depth",
            "0",
            "--min-relevance",
            "0",
        ]),
    )
    .await
    .unwrap();
    let response: Value = serde_json::from_str(plan.rendered_content()).unwrap();
    assert_eq!(response["primary"].as_array().unwrap().len(), 20);
    assert_eq!(response["omitted"]["primary"]["bounded_window_budget"], 0);
    assert!(plan.rendered_content().len() <= DEFAULT_CLI_CONTEXT_BYTES as usize);
    assert!(response["receipt"].is_null());
}
