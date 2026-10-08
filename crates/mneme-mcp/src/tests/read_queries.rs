//! Query/context admission, packing, coverage, and read-only execution.

use crate::*;

#[test]
fn query_schema_exposes_the_exact_graph_off_baseline() {
    let schemas = tool_schemas(CapabilityPolicy::operator());
    let query = schemas
        .iter()
        .find(|tool| tool["name"] == "query")
        .expect("query tool schema");
    assert_eq!(query["inputSchema"]["properties"]["depth"]["minimum"], 0);
    for name in ["query", "recall_context", "ingest", "capture"] {
        let schema = schemas
            .iter()
            .find(|tool| tool["name"] == name)
            .unwrap_or_else(|| panic!("{name} tool schema"));
        assert_eq!(
            schema["inputSchema"]["properties"]["tags"]["items"]["maxLength"],
            mneme_core::MAX_TAG_BYTES,
            "{name}"
        );
    }
}

#[test]
fn public_query_optional_arguments_are_strict_and_never_clamped() {
    for lifecycle_flags in [false, true] {
        for accepted in [1_u64, 64] {
            let arguments = json!({ "text": "q", "k": accepted });
            let parsed = PublicQueryInput::parse(&arguments, lifecycle_flags).unwrap();
            assert_eq!(parsed.k, Some(accepted as usize));
        }
        for rejected in [0_u64, 65, u64::MAX] {
            let error =
                PublicQueryInput::parse(&json!({ "text": "q", "k": rejected }), lifecycle_flags)
                    .err()
                    .unwrap()
                    .to_string();
            assert!(error.contains("`k` must be in 1..=64"), "{error}");
        }
        for invalid in [Value::Null, json!("1"), json!(1.0), json!(true)] {
            let error =
                PublicQueryInput::parse(&json!({ "text": "q", "k": invalid }), lifecycle_flags)
                    .err()
                    .unwrap()
                    .to_string();
            assert!(error.contains("`k` must be an integer"), "{error}");
        }

        for accepted in [0_u64, 1, 12] {
            let arguments = json!({ "text": "q", "depth": accepted });
            let parsed = PublicQueryInput::parse(&arguments, lifecycle_flags).unwrap();
            assert_eq!(parsed.depth, Some(accepted as u8));
        }
        for accepted in [1_u64, 64, 65, 256] {
            let arguments = json!({ "text": "q", "max_nodes": accepted });
            let parsed = PublicQueryInput::parse(&arguments, lifecycle_flags).unwrap();
            assert_eq!(parsed.max_nodes, Some(accepted as usize));
        }
        for accepted in [0.0, 1.0] {
            let arguments = json!({ "text": "q", "min_relevance": accepted });
            let parsed = PublicQueryInput::parse(&arguments, lifecycle_flags).unwrap();
            assert_eq!(parsed.min_relevance, Some(accepted));
        }
        for (key, value) in [
            ("depth", json!(u64::MAX)),
            ("depth", json!(13)),
            ("depth", json!("0")),
            ("max_nodes", json!(0)),
            ("max_nodes", json!(257)),
            ("max_nodes", json!(u64::MAX)),
            ("max_nodes", json!(1.0)),
            ("min_relevance", json!("0.5")),
            ("min_relevance", json!(1.1)),
            ("tags", json!("tag")),
        ] {
            let mut arguments = json!({ "text": "q" });
            arguments[key] = value.clone();
            assert!(
                PublicQueryInput::parse(&arguments, lifecycle_flags).is_err(),
                "{key}={value} must fail"
            );
        }
    }

    for (key, value) in [("archived", json!("false"))] {
        let mut arguments = json!({ "text": "q" });
        arguments[key] = value;
        let error = PublicQueryInput::parse(&arguments, true)
            .err()
            .unwrap()
            .to_string();
        assert!(error.contains("must be a boolean"), "{error}");
    }
}

#[tokio::test]
async fn malformed_query_admission_fails_before_database_checkout() {
    let registry = Registry {
        activity: crate::activity::ActivityRing::default(),
        dbs: BTreeMap::new(),
    };
    let sessions = std::sync::Arc::new(Mutex::new(SessionState::default()));
    let cold_work = ColdWorkGate::new(Duration::ZERO);
    for name in ["query", "recall_context"] {
        let error = call_tool(
            &registry,
            &sessions,
            &cold_work,
            name,
            &json!({
                "db": "does-not-exist",
                "text": "q",
                "k": 65,
            }),
        )
        .await
        .unwrap_err()
        .to_string();
        assert!(error.contains("`k` must be in 1..=64"), "{error}");
        assert!(!error.contains("unknown database"), "{error}");
    }
}

#[tokio::test]
async fn malformed_observe_is_rejected_before_checkout() {
    let registry = Registry {
        activity: crate::activity::ActivityRing::default(),
        dbs: BTreeMap::new(),
    };
    let sessions = std::sync::Arc::new(Mutex::new(SessionState::default()));
    for value in [Value::Null, json!(1), json!("true")] {
        let error = call_tool(
            &registry,
            &sessions,
            &ColdWorkGate::default(),
            "recall_context",
            &json!({"db":"missing", "text":"cue", "observe":value}),
        )
        .await
        .unwrap_err();
        assert!(error.to_string().contains("`observe` must be a boolean"));
    }
    for profile in [
        CapabilityPolicy::new(CapabilityProfile::ReadOnly, false),
        CapabilityPolicy::operator(),
    ] {
        let schema = tool_schemas(profile)
            .into_iter()
            .find(|s| s["name"] == "recall_context")
            .unwrap();
        assert_eq!(
            schema["inputSchema"]["properties"]["observe"]["type"],
            "boolean"
        );
    }
}

#[test]
fn recall_context_schema_is_bounded_and_cannot_widen_lifecycle() {
    let recall_context = tool_schemas(CapabilityPolicy::operator())
        .into_iter()
        .find(|tool| tool["name"] == "recall_context")
        .expect("recall_context tool schema");
    let properties = recall_context["inputSchema"]["properties"]
        .as_object()
        .expect("recall_context properties");
    assert_eq!(properties["k"]["maximum"], MAX_QUERY_K);
    assert_eq!(properties["max_nodes"]["maximum"], MAX_QUERY_NODES);
    assert_eq!(properties["depth"]["maximum"], MAX_QUERY_DEPTH);
    assert_eq!(properties["tags"]["maxItems"], MAX_TAGS);
    assert!(!properties.contains_key("candidates"));
    assert!(!properties.contains_key("archived"));
    assert_eq!(recall_context["inputSchema"]["required"], json!(["text"]));
    let budget = context_presentation_budget(100);
    assert_eq!(budget.max_items(), 200);
    assert_eq!(budget.lanes().primary().max_items(), 100);
    assert_eq!(context_presentation_budget(24).max_items(), 48);
    let smallest = context_presentation_budget(1);
    assert_eq!(smallest.max_items(), 2);
    assert_eq!(smallest.lanes().episodic().target_items(), 1);
    assert_eq!(smallest.lanes().episodic().max_items(), 1);
    assert_eq!(budget.max_content_bytes(), 32 * 1024);
    assert_eq!(budget.lanes().episodic().target_items(), 2);
    assert_eq!(budget.lanes().episodic().max_items(), 100);
    let description = recall_context["description"].as_str().unwrap();
    assert!(description.contains("mneme.context.v7"));
    assert!(description.contains("current lexical"));
    assert!(description.contains("Tag filters skip"));
    assert!(description.contains("same-window repacking"));
    assert!(description.contains("Both lane capacities follow effective max_nodes"));
    assert!(!description.contains("13-card"));
    assert!(!description.contains("final-eight"));
}

#[cfg(all(not(feature = "fastembed"), not(feature = "cozo")))]
#[tokio::test]
async fn recall_context_handler_preserves_the_packers_exact_bounded_text() {
    let root = std::env::temp_dir().join(format!("mneme-recall-context-{}", ulid::Ulid::new()));
    std::fs::create_dir(&root).unwrap();
    let path = root.join("memory.json");
    let slot = std::sync::Arc::new(
        DatabaseSlot::open(
            path.clone(),
            std::sync::Arc::new(host::InferenceRuntime::new()),
            ulid::Ulid::new().to_string(),
        )
        .unwrap(),
    );
    let handle = slot.checkout().unwrap();
    handle
        .mem
        .ingest(Ingest::new(
            "active context result",
            b"body must not be emitted",
            &[],
            Provenance::derived_empty(),
        ))
        .await
        .unwrap();
    handle
        .mem
        .ingest(Ingest::new(
            "probationary context result",
            b"candidate body must not be emitted",
            &[],
            Provenance::derived_empty(),
        ))
        .await
        .unwrap();
    let episode = mneme_app::episode::PreparedEpisode::parse(&json!({
        "action": "append",
        "summary": "context result incident",
        "body": "episode body must not be emitted",
        "source": {"namespace": "mcp-context-test", "key": "incident", "reference": "test://context/episode"},
        "occurred": {"kind": "point", "at": 1000},
        "thread": "bounded-context"
    }))
    .unwrap()
    .run(&handle.mem, handle.db_id, None)
    .await
    .unwrap();
    handle.save().unwrap();
    drop(handle);

    let server = Server {
        registry: Registry {
            activity: crate::activity::ActivityRing::default(),
            dbs: BTreeMap::from([("fixture".into(), slot)]),
        },
        sessions: std::sync::Arc::new(Mutex::new(SessionState::default())),
        cold_work: ColdWorkGate::default(),
        capability: CapabilityPolicy::operator(),
        library: None,
    };
    let arguments = json!({
        "db": "fixture",
        "text": "context result",
        "k": 2,
        "max_nodes": 2,
        "depth": 0,
        "min_relevance": 0.0,
    });
    let query = call_tool(
        &server.registry,
        &server.sessions,
        &server.cold_work,
        "query",
        &arguments,
    )
    .await
    .unwrap();
    assert_eq!(query["schema"], "mneme.query.v3");
    assert_eq!(query["mode"], "untagged");
    assert!(query["lanes"]["primary"]["hits"].is_array());
    assert!(query["lanes"]["primary"]["hits"][0].get("score").is_none());

    let direct = call_tool(
        &server.registry,
        &server.sessions,
        &server.cold_work,
        "recall_context",
        &arguments,
    )
    .await
    .unwrap();
    let packed = direct.as_str().expect("handler returns pre-rendered text");
    let response = server
        .handle(json!({
            "jsonrpc": "2.0",
            "id": "context",
            "method": "tools/call",
            "params": {
                "name": "recall_context",
                "arguments": arguments,
            }
        }))
        .await
        .unwrap();
    assert!(response.bytes().len() < response::MAX_JSONRPC_FRAME_BYTES);
    let frame: Value = serde_json::from_slice(response.bytes()).unwrap();
    assert_eq!(frame["result"]["isError"], false);
    let exact_text = frame["result"]["content"][0]["text"]
        .as_str()
        .expect("one MCP text result");
    assert_eq!(exact_text, packed);
    assert!(exact_text.len() <= (DEFAULT_CONTEXT_BYTES - CONTEXT_CONTROL_RESERVE_BYTES) as usize);
    let envelope: Value = serde_json::from_str(exact_text).unwrap();
    assert_eq!(envelope["schema"], "mneme.context.v7");
    assert_eq!(envelope["retrieval"]["mode"], "untagged");
    assert_eq!(envelope["retrieval"]["partial"], false);
    assert_eq!(envelope["receipt"], Value::Null);
    assert_eq!(envelope["core"], json!([]));
    assert_eq!(envelope["expansions"], json!([]));
    assert_eq!(envelope["primary"].as_array().unwrap().len(), 2);
    assert_eq!(envelope["episodes"].as_array().unwrap().len(), 1);
    assert_eq!(envelope["episodic_retrieval"]["state"], "searched");
    assert_eq!(envelope["episodic_retrieval"]["mode"], "lexical");
    assert_eq!(envelope["episodes"][0]["kind"], "episode");
    assert_eq!(envelope["episodes"][0]["id"], episode["edition_id"]);
    assert_eq!(envelope["episodes"][0]["episode_id"], episode["episode_id"]);
    assert_eq!(
        envelope["episodes"][0]["current_edition_id"],
        episode["edition_id"]
    );
    assert_eq!(
        envelope["episodes"][0]["occurred"],
        json!({"kind": "point", "at": 1000})
    );
    assert!(envelope.get("probationary").is_none());
    assert_eq!(
        envelope["usage"]["content_bytes"].as_u64().unwrap() as usize,
        exact_text.len()
    );
    assert!(!exact_text.contains("body must not be emitted"));

    let page = call_tool(
        &server.registry,
        &server.sessions,
        &server.cold_work,
        "activity",
        &json!({}),
    )
    .await
    .unwrap();
    let events = page["events"].as_array().unwrap();
    assert_eq!(events.len(), 3); // query, direct context, framed context
    let query_ids = ["primary"]
        .into_iter()
        .flat_map(|lane| {
            query["lanes"][lane]["hits"]
                .as_array()
                .unwrap()
                .iter()
                .map(|hit| hit["id"].clone())
        })
        .collect::<Vec<_>>();
    assert_eq!(events[0]["node_ids"], json!(query_ids));
    let context_ids = ["core", "primary", "expansions", "episodes"]
        .into_iter()
        .flat_map(|lane| {
            envelope[lane]
                .as_array()
                .unwrap()
                .iter()
                .map(|hit| hit["id"].clone())
        })
        .collect::<Vec<_>>();
    assert_eq!(events[1]["node_ids"], json!(context_ids));
    assert_eq!(events[2]["node_ids"], json!(context_ids));
    assert_eq!(events[2]["tool"], "recall_context");
    assert_eq!(events[2]["db"], "fixture");
    assert_eq!(
        events[2]["db_id"],
        server
            .registry
            .slot("fixture")
            .unwrap()
            .status()
            .unwrap()
            .db_id
            .to_string()
    );
    assert!(
        !serde_json::to_string(&page)
            .unwrap()
            .contains("context result")
    );

    // Displaying an episode is not exposure or a learning receipt.
    let handle = server.registry.slot("fixture").unwrap().checkout().unwrap();
    for id in &context_ids {
        let id = parse_id(id.as_str().unwrap()).unwrap();
        let node = handle.mem.get_node(id).await.unwrap().unwrap();
        assert_eq!(node.exposure_count(), 0);
        assert_eq!(node.last_exposed(), None);
    }
    drop(handle);

    let id = query_ids[0].as_str().unwrap();
    call_tool(
        &server.registry,
        &server.sessions,
        &server.cold_work,
        "get",
        &json!({"db":"fixture","id":id}),
    )
    .await
    .unwrap();
    let page = call_tool(
        &server.registry,
        &server.sessions,
        &server.cold_work,
        "activity",
        &json!({"after":3}),
    )
    .await
    .unwrap();
    assert_eq!(page["events"][0]["tool"], "get");
    assert_eq!(page["events"][0]["node_ids"], json!([id]));
    assert_eq!(
        server
            .registry
            .slot("fixture")
            .unwrap()
            .status()
            .unwrap()
            .in_flight,
        0
    );

    let mut routed_arguments = arguments.clone();
    routed_arguments["observe"] = json!(true);
    routed_arguments["routing_hints"] = json!([]);
    let routed = call_tool(
        &server.registry,
        &server.sessions,
        &server.cold_work,
        "recall_context",
        &routed_arguments,
    )
    .await
    .unwrap();
    let routed: Value = serde_json::from_str(routed.as_str().unwrap()).unwrap();
    assert_eq!(routed["routing"]["validated"], 0);
    for lane in ["core", "primary", "expansions", "episodes"] {
        assert_eq!(routed[lane], envelope[lane]);
    }
    routed_arguments["routing_hints"] = json!("malformed optional learning");
    let ignored = call_tool(
        &server.registry,
        &server.sessions,
        &server.cold_work,
        "recall_context",
        &routed_arguments,
    )
    .await
    .unwrap();
    let ignored: Value = serde_json::from_str(ignored.as_str().unwrap()).unwrap();
    assert_eq!(ignored["routing"]["ignored"], 1);
    for lane in ["core", "primary", "expansions", "episodes"] {
        assert_eq!(ignored[lane], envelope[lane]);
    }

    let wire_hint = json!({"db_id":ulid::Ulid::from(1u128),"sign":"boost","route":{
        "previous":ulid::Ulid::from(2u128),"target":ulid::Ulid::from(3u128),
        "from":ulid::Ulid::from(2u128),"to":ulid::Ulid::from(3u128),
        "previous_fingerprint":"a".repeat(64),"target_fingerprint":"b".repeat(64),
        "edge_fingerprint":"c".repeat(64)}});
    let mut malformed_opposite = wire_hint.clone();
    malformed_opposite["sign"] = json!("weaken");
    malformed_opposite["route"]["edge_fingerprint"] = json!("A".repeat(64));
    for batch in [
        json!([wire_hint, malformed_opposite]),
        json!(vec![json!({}); mneme_core::ports::MAX_ROUTING_HINTS + 1]),
    ] {
        let count = batch.as_array().unwrap().len();
        routed_arguments["routing_hints"] = batch;
        let neutral = call_tool(
            &server.registry,
            &server.sessions,
            &server.cold_work,
            "recall_context",
            &routed_arguments,
        )
        .await
        .unwrap();
        let neutral: Value = serde_json::from_str(neutral.as_str().unwrap()).unwrap();
        assert_eq!(neutral["routing"]["validated"], 0);
        assert_eq!(neutral["routing"]["ignored"], count);
        assert!(neutral["receipt"].is_null());
        for lane in ["core", "primary", "expansions", "episodes"] {
            assert_eq!(neutral[lane], envelope[lane]);
        }
    }

    let handle = server.registry.slot("fixture").unwrap().checkout().unwrap();
    let optional = recall_context_routed(
        &handle.mem,
        "context result",
        2,
        Budget {
            max_nodes: 2,
            max_depth: 0,
            min_relevance: 0.0,
            ..handle.mem.config().budget
        },
        &[],
        &context_presentation_budget(100),
        &[],
    )
    .await
    .unwrap();
    let small_limit = optional.plan.rendered_content().len() + 150;
    let overflow = context_observation::render_routed(
        &optional.plan,
        Some(&optional.observations),
        handle.db_id,
        optional.routing.as_ref().unwrap(),
        small_limit,
    )
    .unwrap_err();
    assert!(
        overflow
            .downcast_ref::<context_observation::EnvelopeOverflow>()
            .is_some()
    );

    drop(handle);

    let before_shadow = std::fs::read(&path).unwrap();
    let mut observed_arguments = arguments.clone();
    observed_arguments["observe"] = json!(true);
    let observed = call_tool(
        &server.registry,
        &server.sessions,
        &server.cold_work,
        "recall_context",
        &observed_arguments,
    )
    .await
    .unwrap();
    let observed_text = observed.as_str().unwrap();
    let observed: Value = serde_json::from_str(observed_text).unwrap();
    for lane in ["core", "primary", "expansions", "episodes"] {
        assert_eq!(
            observed[lane], envelope[lane],
            "observing does not change cards"
        );
    }
    assert_eq!(observed["observation"]["schema"], 1);
    assert_eq!(observed["observation"]["learning"], "disabled");
    assert_eq!(observed["usage"]["content_bytes"], observed_text.len());
    assert!(observed_text.len() <= DEFAULT_CONTEXT_BYTES as usize);
    assert!(observed["receipt"].is_null());
    let observations = observed["observation"]["cards"].as_array().unwrap();
    assert_eq!(observations.len(), context_ids.len());
    assert!(observations.iter().all(|card| card["graph_path"].is_null()));
    assert!(
        observations
            .iter()
            .all(|card| card["card_sha256"].as_str().unwrap().len() == 64)
    );
    for observation in observations {
        let card = ["core", "primary", "expansions", "episodes"]
            .into_iter()
            .flat_map(|lane| observed[lane].as_array().unwrap())
            .find(|card| card["id"] == observation["node_id"])
            .unwrap();
        assert_eq!(
            observation["card_sha256"],
            format!("{:x}", Sha256::digest(serde_json::to_vec(card).unwrap()))
        );
    }
    assert_eq!(server.sessions.lock().await.receipt_count(), 0);
    assert_eq!(std::fs::read(&path).unwrap(), before_shadow);
    observed_arguments["observe"] = json!(false);
    let ordinary = call_tool(
        &server.registry,
        &server.sessions,
        &server.cold_work,
        "recall_context",
        &observed_arguments,
    )
    .await
    .unwrap();
    assert_eq!(ordinary.as_str().unwrap(), packed);

    drop(server);
    std::fs::remove_dir_all(root).unwrap();
}

#[cfg(all(not(feature = "fastembed"), not(feature = "cozo")))]
#[tokio::test]
async fn exact_tagged_query_and_context_share_one_primary_lane() {
    let root = std::env::temp_dir().join(format!("mneme-tagged-context-{}", ulid::Ulid::new()));
    std::fs::create_dir(&root).unwrap();
    let path = root.join("memory.json");
    let slot = std::sync::Arc::new(
        DatabaseSlot::open(
            path,
            std::sync::Arc::new(host::InferenceRuntime::new()),
            ulid::Ulid::new().to_string(),
        )
        .unwrap(),
    );
    let server = Server {
        registry: Registry {
            activity: crate::activity::ActivityRing::default(),
            dbs: BTreeMap::from([("fixture".into(), slot)]),
        },
        sessions: std::sync::Arc::new(Mutex::new(SessionState::default())),
        cold_work: ColdWorkGate::default(),
        capability: CapabilityPolicy::operator(),
        library: None,
    };
    let tag = "é".repeat(mneme_core::MAX_TAG_BYTES / "é".len());
    assert_eq!(tag.len(), mneme_core::MAX_TAG_BYTES);
    call_tool(
        &server.registry,
        &server.sessions,
        &server.cold_work,
        "ingest",
        &json!({
            "db": "fixture",
            "summary": "exact tagged active",
            "tags": [tag.clone()],
        }),
    )
    .await
    .unwrap();
    let candidate = call_tool(
        &server.registry,
        &server.sessions,
        &server.cold_work,
        "ingest",
        &json!({
            "db": "fixture",
            "summary": "exact tagged candidate",
            "tags": [tag.clone()],
        }),
    )
    .await
    .unwrap();
    for oversized in ["x".repeat(257), "é".repeat(129)] {
        let error = call_tool(
            &server.registry,
            &server.sessions,
            &server.cold_work,
            "ingest",
            &json!({
                "db": "fixture",
                "summary": "oversized tag must fail",
                "tags": [oversized],
            }),
        )
        .await
        .unwrap_err()
        .to_string();
        assert!(error.contains("maximum is 256"), "{error}");
    }
    let arguments = json!({
        "db": "fixture",
        "text": "exact tagged",
        "tags": [tag],
        "k": 2,
        "max_nodes": 2,
        "depth": 0,
        "min_relevance": 0.0,
    });
    let query = call_tool(
        &server.registry,
        &server.sessions,
        &server.cold_work,
        "query",
        &arguments,
    )
    .await
    .unwrap();
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
    assert!(
        query["lanes"]["primary"]["hits"]
            .as_array()
            .unwrap()
            .iter()
            .any(|hit| hit["id"] == candidate["id"])
    );
    assert_eq!(query["work"]["raw_memberships"], 2);
    assert_eq!(query["work"]["exact_hydrated_ids"], 2);
    assert_eq!(
        query["stamp"]["projection_watermarks"][0]["projection"],
        "tag-membership"
    );
    let watermark = query["stamp"]["projection_watermarks"][0]["watermark"]
        .as_str()
        .unwrap()
        .to_owned();

    let packed = call_tool(
        &server.registry,
        &server.sessions,
        &server.cold_work,
        "recall_context",
        &arguments,
    )
    .await
    .unwrap();
    let packed = packed.as_str().unwrap();
    let context: Value = serde_json::from_str(packed).unwrap();
    assert_eq!(context["schema"], "mneme.context.v7");
    assert_eq!(context["retrieval"]["mode"], "tagged");
    assert_eq!(
        context["episodic_retrieval"]["state"],
        "not_searched_tag_filter"
    );
    assert_eq!(context["episodes"], json!([]));
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
        packed.len()
    );

    drop(server);
    std::fs::remove_dir_all(root).unwrap();
}

#[cfg(all(not(feature = "fastembed"), not(feature = "cozo")))]
#[tokio::test]
async fn popular_tag_partial_coverage_is_reported_without_reserved_slot() {
    use mneme_core::ports::{Embedder, EmbeddingMetadataStore};

    const DIM: usize = 8;
    let root = std::env::temp_dir().join(format!("mneme-popular-context-{}", ulid::Ulid::new()));
    std::fs::create_dir(&root).unwrap();
    let path = root.join("memory.json");
    let store = std::sync::Arc::new(mneme_cozo::MemStore::new(DIM));
    let embedder = std::sync::Arc::new(mneme_embed::HashingEmbedder::new(DIM));
    store
        .set_embedding_fingerprint(&embedder.fingerprint())
        .unwrap();
    let mem = mneme_engine::Memory::new(
        store.clone(),
        store.clone(),
        store.clone(),
        embedder,
        std::sync::Arc::new(mneme_core::ports::SystemClock),
        mneme_engine::Config {
            ann_k: 2,
            lexical_k: 0,
            graph_seed_cap: 0,
            graph_slot_cap: 0,
            similarity_link_cap: 0,
            min_similarity_links: 0,
            ..mneme_engine::Config::default()
        },
    )
    .with_body_store(std::sync::Arc::new(mneme_body::InlineStore::new()));
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
    let _candidate = mem
        .ingest(Ingest::new(
            "public popular candidate",
            b"",
            &[tag],
            Provenance::derived_empty(),
        ))
        .await
        .unwrap();
    drop(mem);
    store.save(&path).unwrap();
    drop(store);

    let slot = std::sync::Arc::new(
        DatabaseSlot::open(
            path,
            std::sync::Arc::new(host::InferenceRuntime::new()),
            ulid::Ulid::new().to_string(),
        )
        .unwrap(),
    );
    let server = Server {
        registry: Registry {
            activity: crate::activity::ActivityRing::default(),
            dbs: BTreeMap::from([("fixture".into(), slot)]),
        },
        sessions: std::sync::Arc::new(Mutex::new(SessionState::default())),
        cold_work: ColdWorkGate::default(),
        capability: CapabilityPolicy::operator(),
        library: None,
    };
    let arguments = json!({
        "db": "fixture",
        "text": "public popular candidate",
        "tags": [tag],
        "k": 2,
        "max_nodes": 2,
        "depth": 0,
        "min_relevance": 0.0,
    });
    let query = call_tool(
        &server.registry,
        &server.sessions,
        &server.cold_work,
        "query",
        &arguments,
    )
    .await
    .unwrap();
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

    let packed = call_tool(
        &server.registry,
        &server.sessions,
        &server.cold_work,
        "recall_context",
        &arguments,
    )
    .await
    .unwrap();
    let packed = packed.as_str().unwrap();
    let context: Value = serde_json::from_str(packed).unwrap();
    assert_eq!(context["schema"], "mneme.context.v7");
    assert_eq!(context["retrieval"]["mode"], "tagged");
    assert_eq!(
        context["episodic_retrieval"]["state"],
        "not_searched_tag_filter"
    );
    assert_eq!(context["episodes"], json!([]));
    assert_eq!(context["retrieval"]["partial"], true);
    assert_eq!(context["retrieval"]["work"], query["work"]);
    assert_eq!(
        context["retrieval"]["stamp"]["projection_watermarks"][0]["watermark"],
        watermark
    );
    assert!(context.get("probationary").is_none());
    assert_eq!(
        context["usage"]["content_bytes"].as_u64().unwrap() as usize,
        packed.len()
    );

    drop(server);
    std::fs::remove_dir_all(root).unwrap();
}

#[cfg(all(not(feature = "fastembed"), not(feature = "cozo"), unix))]
#[tokio::test]
async fn query_and_recall_do_not_expose_or_checkpoint_when_query_output_is_oversize() {
    use std::os::unix::fs::MetadataExt;

    let root = std::env::temp_dir().join(format!("mneme-pure-query-{}", ulid::Ulid::new()));
    std::fs::create_dir(&root).unwrap();
    let path = root.join("memory.json");
    let slot = std::sync::Arc::new(
        DatabaseSlot::open(
            path.clone(),
            std::sync::Arc::new(host::InferenceRuntime::new()),
            ulid::Ulid::new().to_string(),
        )
        .unwrap(),
    );
    let handle = slot.checkout().unwrap();
    let mut ids = Vec::new();
    for index in 0..MAX_QUERY_K {
        // Quotes are one byte in the node but two bytes in compact JSON. All
        // 64 complete cards fit the work budget while exceeding the MCP text
        // fuse, exercising the dangerous post-retrieval failure path.
        let summary = format!(
            "{index:02}{}",
            "\"".repeat(MAX_SUMMARY_BYTES.saturating_sub(2))
        );
        let id = handle
            .mem
            .ingest(Ingest::new(&summary, b"", &[], Provenance::derived_empty()))
            .await
            .unwrap();
        ids.push(id);
    }
    for &id in &ids {
        assert!(
            handle.mem.neighbors(id, 1).await.unwrap().is_empty(),
            "fixture starts without topology"
        );
    }
    handle.save().unwrap();
    drop(handle);
    let baseline_bytes = std::fs::read(&path).unwrap();
    let baseline_inode = std::fs::metadata(&path).unwrap().ino();

    let server = Server {
        registry: Registry {
            activity: crate::activity::ActivityRing::default(),
            dbs: BTreeMap::from([("fixture".into(), slot)]),
        },
        sessions: std::sync::Arc::new(Mutex::new(SessionState::default())),
        cold_work: ColdWorkGate::default(),
        capability: CapabilityPolicy::operator(),
        library: None,
    };
    // Keep the response-amplifying punctuation, but include one real token:
    // the hashing embedder intentionally maps punctuation-only input to the
    // zero vector, which is not a valid cosine query.
    let query = format!("token{}", "\"".repeat(MAX_SUMMARY_BYTES));
    let response = server
        .handle(json!({
            "jsonrpc": "2.0",
            "id": 1,
            "method": "tools/call",
            "params": {
                "name": "query",
                "arguments": {
                    "db": "fixture",
                    "text": query,
                    "k": MAX_QUERY_K,
                    "max_nodes": MAX_QUERY_K,
                    "depth": 0,
                    "min_relevance": 0.0,
                }
            }
        }))
        .await
        .unwrap();
    let frame: Value = serde_json::from_slice(response.bytes()).unwrap();
    assert_eq!(frame["result"]["isError"], true);
    assert!(
        frame["result"]["content"][0]["text"]
            .as_str()
            .is_some_and(|text| text.contains("tool result is")),
        "fixture must reach the post-work decoded-text fuse: {frame}"
    );
    assert_eq!(std::fs::read(&path).unwrap(), baseline_bytes);
    assert_eq!(
        std::fs::metadata(&path).unwrap().ino(),
        baseline_inode,
        "an oversize query must not atomically replace an unchanged snapshot"
    );

    let recall = server
        .handle(json!({
            "jsonrpc": "2.0",
            "id": 2,
            "method": "tools/call",
            "params": {
                "name": "recall",
                "arguments": {
                    "db": "fixture",
                    "text": "00",
                    "expand_top": 1,
                    "neighbors_each": 1,
                }
            }
        }))
        .await
        .unwrap();
    let recall_frame: Value = serde_json::from_slice(recall.bytes()).unwrap();
    assert_eq!(recall_frame["result"]["isError"], false);
    assert_eq!(std::fs::read(&path).unwrap(), baseline_bytes);
    assert_eq!(std::fs::metadata(&path).unwrap().ino(), baseline_inode);

    let handle = server.registry.checkout("fixture").unwrap();
    let mem = &handle.mem;
    for id in ids {
        let node = mem.get_node(id).await.unwrap().unwrap();
        assert_eq!(node.exposure_count(), 0);
        assert_eq!(node.last_exposed(), None);
        assert!(
            mem.neighbors(id, 1).await.unwrap().is_empty(),
            "query/recall planning must not learn topology"
        );
    }

    drop(handle);
    drop(server);
    std::fs::remove_dir_all(root).unwrap();
}
