use super::*;

const UNIT: [f32; 8] = [1., 0., 0., 0., 0., 0., 0., 0.];

fn toy() -> Case {
    Case {
        id: "toy".into(),
        title: "Synthetic lane saturation".into(),
        checkpoints: vec![Checkpoint {
            id: "start".into(),
            nodes: (1..=8)
                .map(|id| NodeRow {
                    id,
                    summary: format!("Synthetic note {id}"),
                    body: format!("Body {id}"),
                    status: if id == 7 {
                        "candidate"
                    } else if id == 8 {
                        "archived"
                    } else {
                        "ordinary"
                    }
                    .into(),
                    candidate_use_count: (id == 7).then_some(0),
                    confidence: 0.8,
                    stability: 0.5,
                    vector: UNIT,
                })
                .collect(),
            edges: vec![EdgeRow {
                from: 1,
                to: 8,
                kind: EdgeKind::Transition,
                anchor: None,
                weight: 0.7,
                last_reinforced: 100,
                trials: 2,
                interference: 1,
            }],
            contradictions: vec![],
            declared_changes_from_previous: vec![],
        }],
        queries: vec![Query {
            id: "q".into(),
            text: "Synthetic query".into(),
            vector: UNIT,
        }],
        runs: vec![Run {
            id: "r".into(),
            checkpoint: "start".into(),
            query_id: "q".into(),
            repeat: 2,
        }],
        observed_ids: BTreeMap::from([("watched".into(), vec![7, 8])]),
        extra_integrity_checks: vec![json!({"kind":"identical_repeated_read_results","run":"r"})],
    }
}

fn packing_json() -> Value {
    let reserve = PresentationBudget::minimum_control_reserve_bytes();
    let lane = |hard, target, max| {
        json!({"hard_min_items":hard,"target_items":target,
        "max_items":max,"max_bytes":8192-reserve})
    };
    json!({"max_content_bytes":8192,"control_reserve_bytes":reserve,"max_items":6,
        "max_summary_bytes_each":256,"normal_content_limit":8192-reserve,
        "expected_native_minimum_control_reserve_bytes":reserve,"lane_byte_caps_are_additive":false,
        "body":{"max_fetches":0,"max_source_bytes_total":0,"max_rendered_bytes_total":0,"max_source_bytes_each":0},
        "lanes":{"core":lane(0,0,0),"primary":lane(1,5,6),"probationary":lane(0,1,1),
            "expansion":lane(0,0,0),"episodic":lane(0,0,0)}})
}

fn controls() -> Controls {
    let budget = Budget {
        max_nodes: 16,
        max_depth: 0,
        min_relevance: 0.0,
        ..Budget::default()
    };
    Controls {
        cfg: Config {
            ann_k: 6,
            lexical_k: 0,
            graph_seed_cap: 0,
            budget,
            ..Config::default()
        },
        budget,
        packing: presentation(&packing_json()).unwrap(),
        k: 6,
        status: StatusFilter::ACTIVE,
        now: 100,
        fingerprint: EmbeddingFingerprint::new("synthetic-test", 8, "unit", "exact-text"),
    }
}

fn config_json() -> Value {
    let c = controls().cfg;
    let mut v = json!({"default_body_scheme":"inline"});
    macro_rules! fields { ($($key:ident),* $(,)?) => { $(v[stringify!($key)] = json!(c.$key);)* }; }
    fields!(
        ann_k,
        lexical_k,
        rrf_constant,
        dense_weight,
        lexical_weight,
        graph_seed_cap,
        graph_weight,
        graph_slot_cap,
        candidate_admission_limit,
        similarity_link_cap,
        similarity_link_threshold,
        min_similarity_links,
        coretrieval_link_cap,
        confidence_interference_retention,
        confidence_restore,
        confidence_floor,
        promote_use_threshold,
        archive_floor,
        bridge_probability,
        bridge_weight,
        prune_weight_floor,
        dense_degree_threshold
    );
    for (key, s) in [
        ("strength", c.strength),
        ("bridge_strength", c.bridge_strength),
    ] {
        v[key] = json!({"reinforce_gain":s.reinforce_gain,"interference_retention":s.interference_retention,
            "interference_resist":s.interference_resist,"resist_trials_half":s.resist_trials_half});
    }
    let b = c.budget;
    v["budget"] = json!({"max_nodes":b.max_nodes,"max_depth":b.max_depth,"min_relevance":b.min_relevance,
        "explore":b.explore,"relevance_ratio":b.relevance_ratio,"dedup_similarity":b.dedup_similarity,
        "query_conditioning":b.query_conditioning});
    v
}

#[test]
fn vector_scalar_and_status_validation() {
    assert!(vector(&UNIT).is_ok());
    for x in [0.0, 0.5, f32::NAN, f32::INFINITY] {
        let mut v = UNIT;
        v[0] = x;
        assert!(vector(&v).is_err());
    }
    for bad in [-0.1, 1.1, f32::NAN, f32::INFINITY] {
        for stability in [false, true] {
            let mut c = toy();
            let n = &mut c.checkpoints[0].nodes[0];
            if stability {
                n.stability = bad;
            } else {
                n.confidence = bad;
            }
            assert!(validate_case(&c).is_err());
        }
    }
    for (label, count) in [
        ("candidate", None),
        ("candidate", Some(1)),
        ("ordinary", Some(0)),
        ("archived", Some(0)),
        ("Active", None),
    ] {
        let mut n = toy().checkpoints.remove(0).nodes.remove(0);
        n.status = label.into();
        n.candidate_use_count = count;
        assert!(status(&n).is_err());
    }
    assert!(validate_case(&toy()).is_ok());
}

#[test]
fn schema_references_and_bounds_fail_closed() {
    let mut v = serde_json::to_value(toy()).unwrap();
    v["gold_answer"] = json!(7);
    assert!(serde_json::from_value::<Case>(v).is_err());
    let mut v = serde_json::to_value(toy()).unwrap();
    v["checkpoints"][0]["nodes"][0]["vector"] = json!([1, 0]);
    assert!(serde_json::from_value::<Case>(v).is_err());
    let mutations: [fn(&mut Case); 8] = [
        |c| c.checkpoints[0].nodes[1].id = 1,
        |c| c.checkpoints[0].nodes[0].id = u64::from(u32::MAX) + 1,
        |c| c.checkpoints[0].nodes[0].body = "x".repeat(513),
        |c| c.checkpoints[0].edges[0].to = 99,
        |c| c.checkpoints[0].edges[0].weight = f32::NAN,
        |c| c.runs[0].repeat = 0,
        |c| c.runs[0].query_id = "missing".into(),
        |c| {
            c.observed_ids.insert("absent".into(), vec![99]);
        },
    ];
    for mutate in mutations {
        let mut c = toy();
        mutate(&mut c);
        assert!(validate_case(&c).is_err());
    }
}

#[test]
fn native_config_and_presentation_parse_explicit_controls() {
    let mut v = config_json();
    v["graph_seed_cap"] = json!(7);
    let parsed = config(&v).unwrap();
    assert_eq!(parsed.graph_seed_cap, 7);
    assert_eq!(parsed.candidate_admission_limit, 1);
    assert_eq!(parsed.budget.max_depth, 0);
    v.as_object_mut().unwrap().remove("archive_floor");
    assert!(config(&v).is_err());
    let p = presentation(&packing_json()).unwrap();
    assert_eq!((p.max_items(), p.max_content_bytes()), (6, 8192));
    assert_eq!(p.lanes().probationary().target_items(), 1);
    assert_eq!(p.body(), BodyBudget::disabled());
    for (pointer, bad) in [
        ("/max_items", json!(0)),
        ("/lanes/primary/target_items", json!(0)),
        ("/normal_content_limit", json!(8192)),
        ("/lane_byte_caps_are_additive", json!(true)),
        ("/body/max_fetches", json!(1)),
        ("/expected_native_minimum_control_reserve_bytes", json!(0)),
    ] {
        let mut v = packing_json();
        *v.pointer_mut(pointer).unwrap() = bad;
        assert!(presentation(&v).is_err(), "{pointer}");
    }
}

#[test]
fn numeric_ids_are_lossless_and_checked_on_output() {
    for id in [0, 1, u64::from(u32::MAX), u64::MAX] {
        assert_eq!(number(nid(id)).unwrap(), id);
    }
    assert!(number(NodeId(u128::MAX.into())).is_err());
    assert_ne!(nid(1), nid(2));
}

#[tokio::test]
async fn scripted_embedding_requires_exact_declared_unambiguous_text() {
    let c = toy();
    let ctrl = controls();
    let e = Scripted::new(&c, ctrl.fingerprint.clone()).unwrap();
    assert_eq!(e.dim(), 8);
    assert_eq!(e.fingerprint(), ctrl.fingerprint);
    assert_eq!(
        e.embed(&[&c.queries[0].text]).await.unwrap(),
        vec![UNIT.to_vec()]
    );
    assert!(e.embed(&["synthetic query"]).await.is_err());
    assert!(e.embed(&[&c.queries[0].text, "undeclared"]).await.is_err());
    let mut conflict = c.clone();
    conflict.queries[0].text = c.checkpoints[0].nodes[0].summary.clone();
    assert!(Scripted::new(&conflict, ctrl.fingerprint.clone()).is_ok());
    conflict.queries[0].vector = [0., 1., 0., 0., 0., 0., 0., 0.];
    assert!(Scripted::new(&conflict, ctrl.fingerprint).is_err());
}

#[tokio::test]
async fn normalization_changes_only_candidate_status_and_detached_state_survives() {
    let (base, _) = base_snapshot(&toy().checkpoints[0], &controls())
        .await
        .unwrap();
    let original = canonical(&base).unwrap();
    let mut changed = normalized(base.clone());
    assert!(
        changed
            .nodes
            .iter()
            .find(|n| n.id() == nid(7))
            .unwrap()
            .is_active()
    );
    changed
        .nodes
        .iter_mut()
        .find(|n| n.id() == nid(7))
        .unwrap()
        .set_status(NodeStatus::Candidate { use_count: 0 });
    assert_eq!(canonical(&changed).unwrap(), original);
    let detached = MemStore::from_export(base.clone()).unwrap();
    assert_eq!(canonical(&detached.export()).unwrap(), original);
    let mut reordered = base.clone();
    reordered.nodes.reverse();
    reordered.edges.reverse();
    assert_eq!(canonical(&reordered).unwrap(), original);
    assert_eq!(
        state_sha(&detached).unwrap(),
        sha(serde_json::to_vec(&original).unwrap())
    );
}

#[tokio::test]
async fn native_packing_preserves_probation_and_queries_are_repeatable_and_pure() {
    let mut calls = 0;
    let mut stamps = BTreeMap::new();
    let report = run_case(&toy(), &controls(), &mut stamps, &mut calls)
        .await
        .unwrap();
    assert_eq!(calls, 8); // Two arms, two repeats, raw and native presentation each.
    assert!(!stamps.is_empty());
    for arm in ["current", "normalized"] {
        let a = &report["checkpoints"][0]["arms"][arm];
        assert_eq!(a["state_before_sha256"], a["state_after_sha256"]);
        assert_eq!(a["runs"][0]["repeat"], 2);
        let sample = &a["runs"][0]["sample"];
        assert_eq!(sample["delivered"].as_array().unwrap().len(), 6);
        assert!(!raw_ids(sample).contains(&8));
        forbid_archived(sample, &BTreeSet::from([8])).unwrap();
        assert!(sample["context_bytes"].as_u64().unwrap() <= 8192);
    }
    let sample = &report["checkpoints"][0]["arms"]["current"]["runs"][0]["sample"];
    assert_eq!(sample["raw"]["primary"].as_array().unwrap().len(), 6);
    assert_eq!(sample["raw"]["probationary"][0]["id"], 7);
    // A primary-first concatenation truncated at six would silently lose ID 7.
    assert!(delivered_ids(sample).contains(&7));
    let card = sample["delivered"]
        .as_array()
        .unwrap()
        .iter()
        .find(|v| v["id"] == 7)
        .unwrap();
    assert_eq!(card["lane"], "probationary");
}

#[test]
fn archived_guard_checks_every_visible_lane_and_route_endpoint() {
    let archived = BTreeSet::from([8]);
    for pointer in ["/raw/primary", "/raw/probationary", "/delivered"] {
        let mut v = json!({"raw":{"primary":[],"probationary":[]},"delivered":[]});
        *v.pointer_mut(pointer).unwrap() = json!([{"id":8,"winning_path":null}]);
        assert!(forbid_archived(&v, &archived).is_err());
        for name in ["previous", "target", "edge_from", "edge_to"] {
            let mut hop = json!({"previous":1,"target":2,"edge_from":1,"edge_to":2});
            hop[name] = json!(8);
            *v.pointer_mut(pointer).unwrap() = json!([{"id":1,"winning_path":[hop]}]);
            assert!(forbid_archived(&v, &archived).is_err());
        }
    }
}

#[test]
fn hashes_and_byte_caps_are_exact_and_fail_without_truncation() {
    assert_eq!(
        sha(b"abc"),
        "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
    );
    assert!(checked_inputs(b"{}", b"{}").is_err());
    assert!(
        checked_inputs(&vec![0; INPUT_CAP + 1], b"{}")
            .unwrap_err()
            .to_string()
            .contains("cap")
    );
    let value = json!("x".repeat(OUTPUT_CAP - 3));
    let bytes = bounded_output(&value).unwrap();
    assert_eq!(bytes.len(), OUTPUT_CAP);
    assert_eq!(bytes.last(), Some(&b'\n'));
    assert_eq!(serde_json::from_slice::<Value>(&bytes).unwrap(), value);
    assert!(bounded_output(&json!("x".repeat(OUTPUT_CAP - 2))).is_err());
}
