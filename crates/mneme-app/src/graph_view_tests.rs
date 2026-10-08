use super::*;
use mneme_core::ports::{GraphStore, SystemClock, Traversal, VectorIndex};
use mneme_core::{
    BodyRef, BoundedTagSet, Confidence, Edge, NodeInit, NodeSummary, Provenance, Stability,
};
use mneme_cozo::MemStore;
use mneme_embed::{DEFAULT_DIM, HashingEmbedder};
use mneme_engine::Config;
use std::sync::Arc;

fn memory<T: GraphStore + VectorIndex + Traversal + 'static>(store: Arc<T>) -> Memory {
    Memory::new(
        store.clone(),
        store.clone(),
        store,
        Arc::new(HashingEmbedder::new(DEFAULT_DIM)),
        Arc::new(SystemClock),
        Config::default(),
    )
    .with_body_store(Arc::new(mneme_body::InlineStore::new()))
}
fn node(index: u128, summary: &str, tags: Vec<String>) -> Node {
    Node::new(NodeInit {
        id: NodeId(Ulid(index)),
        summary: NodeSummary::new(summary).unwrap(),
        body: BodyRef::new("file:///never-read-body").unwrap(),
        tags: BoundedTagSet::try_from_iter(tags).unwrap(),
        provenance: Provenance::Conversation {
            session: Ulid(0),
            turn: 0,
        },
        stability: Stability::new(0.5).unwrap(),
        confidence: Confidence::new(0.5).unwrap(),
        status: if index % 2 == 0 {
            NodeStatus::Archived
        } else {
            NodeStatus::Active
        },
        created: index,
    })
}
async fn run(mem: &Memory, args: Value) -> Value {
    PreparedGraph::parse(&args)
        .unwrap()
        .run(mem, Ulid(9999))
        .await
        .unwrap()
}
#[test]
fn admission_is_bounded_closed_and_forwardable() {
    assert_eq!(
        PreparedGraph::parse(&json!({"action":"topology"}))
            .unwrap()
            .into_json(),
        json!({"action":"topology","limit":256})
    );
    for bad in [
        json!({}),
        json!({"action":"topology","limit":257}),
        json!({"action":"topology","limit":0}),
        json!({"action":"topology","after":null}),
        json!({"action":"topology","body":true}),
        json!({"action":"summaries","ids":[]}),
        json!({"action":"summaries","ids":[Ulid(1),Ulid(1)]}),
        json!({"action":"summaries","ids":(1..=65).map(Ulid).collect::<Vec<_>>()}),
        json!({"action":"summaries","ids":[Ulid(1)],"limit":1}),
    ] {
        assert!(PreparedGraph::parse(&bad).is_err(), "accepted {bad}");
    }
}
async fn check_topology<T: GraphStore + VectorIndex + Traversal + 'static>(store: Arc<T>) {
    let mem = memory(store.clone());
    for index in 1..=300 {
        store
            .put_node(&node(
                index,
                "Never exported by topology",
                vec!["authored-tag".into()],
            ))
            .await
            .unwrap();
    }
    for index in 1..=100 {
        store
            .put_edge(&Edge::new(
                NodeId(Ulid(index)),
                NodeId(Ulid(index + 1)),
                0.5,
                EdgeKind::Associative,
                index,
            ))
            .await
            .unwrap();
    }
    let mut args = json!({"action":"topology","limit":256});
    let mut ids = Vec::new();
    let mut edges = Vec::new();
    let mut calls = 0;
    loop {
        let response = run(&mem, args.clone()).await;
        assert!(serde_json::to_vec(&response).unwrap().len() <= MAX_GRAPH_PAGE_BYTES);
        assert!(response["coverage"]["page_records"].as_u64().unwrap() <= 256);
        assert_eq!(response["coverage"]["snapshot"], false);
        for item in response["nodes"].as_array().unwrap() {
            assert!(item.get("summary").is_none() && item.get("body").is_none());
            assert_eq!(item["tags"], json!(["authored-tag"]));
            assert_eq!(item["tags_truncated"], false);
            ids.push(item["id"].clone());
        }
        for item in response["edges"].as_array().unwrap() {
            edges.push((item["from"].clone(), item["to"].clone()));
        }
        calls += 1;
        if response["next_cursor"].is_null() {
            assert_eq!(response["coverage"]["nodes_seen"], 300);
            assert_eq!(response["coverage"]["edges_seen"], 100);
            break;
        }
        args["after"] = response["next_cursor"].clone();
        assert!(calls < 4);
    }
    assert_eq!(calls, 2);
    assert_eq!(ids.len(), 300);
    assert_eq!(edges.len(), 100);
    assert!(ids.contains(&json!(Ulid(300)))); // isolated identity survives
    assert!(
        ids.windows(2)
            .all(|pair| pair[0].as_str() < pair[1].as_str())
    );
    let unchanged = mem.get_node(NodeId(Ulid(1))).await.unwrap().unwrap();
    assert_eq!(unchanged.exposure_count(), 0);
    assert_eq!(unchanged.last_exposed(), None);
}
#[tokio::test]
async fn topology_is_complete_global_indexed_isolated_and_read_only() {
    check_topology(Arc::new(MemStore::new(DEFAULT_DIM))).await;
}

#[tokio::test]
async fn highwaters_are_not_snapshots_and_cursors_bind_database() {
    let store = Arc::new(MemStore::new(DEFAULT_DIM));
    let mem = memory(store.clone());
    for index in 1..=4 {
        store.put_node(&node(index, "Node", vec![])).await.unwrap();
    }
    store
        .put_edge(&Edge::new(
            NodeId(Ulid(1)),
            NodeId(Ulid(2)),
            0.5,
            EdgeKind::Bridge,
            0,
        ))
        .await
        .unwrap();
    let first = run(&mem, json!({"action":"topology","limit":1})).await;
    let cursor = first["next_cursor"].clone();
    assert!(
        PreparedGraph::parse(&json!({"action":"topology","after":cursor}))
            .unwrap()
            .run(&mem, Ulid(9))
            .await
            .unwrap_err()
            .to_string()
            .contains("identity mismatch")
    );
    store.delete_node(NodeId(Ulid(2))).await.unwrap();
    store.put_node(&node(5, "Later", vec![])).await.unwrap();
    store
        .put_edge(&Edge::new(
            NodeId(Ulid(4)),
            NodeId(Ulid(5)),
            0.5,
            EdgeKind::Bridge,
            0,
        ))
        .await
        .unwrap();
    let next = run(&mem, json!({"action":"topology","after":cursor})).await;
    assert_eq!(next["nodes"].as_array().unwrap().len(), 2);
    assert_eq!(next["coverage"]["nodes_seen"], 3);
    assert!(
        next["nodes"]
            .as_array()
            .unwrap()
            .iter()
            .all(|item| item["id"] != json!(Ulid(5)))
    );
    assert_eq!(next["edges"].as_array().unwrap().len(), 1);
    assert_eq!(next["has_more"], false);
    let fresh = run(&mem, json!({"action":"topology"})).await;
    assert_eq!(fresh["coverage"]["nodes_seen"], 4);
    assert_eq!(fresh["coverage"]["edges_seen"], 2);
}
#[tokio::test]
async fn summaries_keep_exact_slots_and_bound_escaped_json_tags_and_utf8() {
    let store = Arc::new(MemStore::new(DEFAULT_DIM));
    let mem = memory(store.clone());
    let tags = (0..64)
        .map(|index| format!("{index:02}{}", "\"".repeat(254)))
        .collect::<Vec<_>>();
    for index in 1..=64 {
        store
            .put_node(&node(index, &"\"é".repeat(1000), tags.clone()))
            .await
            .unwrap();
    }
    let response = run(
        &mem,
        json!({"action":"summaries","ids":(1..=64).map(Ulid).collect::<Vec<_>>()}),
    )
    .await;
    assert!(serde_json::to_vec(&response).unwrap().len() <= MAX_GRAPH_PAGE_BYTES);
    for card in response["items"].as_array().unwrap() {
        assert_eq!(card["summary_truncated"], true);
        assert_eq!(card["tags_truncated"], true);
        assert!(card.get("body").is_none());
        assert!(card["summary"].as_str().unwrap().len() <= MAX_GRAPH_SUMMARY_BYTES);
        assert!(serde_json::to_vec(&card["tags"]).unwrap().len() <= MAX_TAGS_JSON_BYTES);
    }
    let exact = run(
        &mem,
        json!({"action":"summaries","ids":[Ulid(64),Ulid(99),Ulid(1)]}),
    )
    .await;
    assert_eq!(exact["items"][0]["id"], json!(Ulid(64)));
    assert_eq!(exact["items"][1], json!({"id":Ulid(99),"missing":true}));
    assert_eq!(exact["items"][2]["id"], json!(Ulid(1)));
}
#[tokio::test]
async fn historical_editions_keep_identity_in_topology_and_summary_batches() {
    use crate::episode::PreparedEpisode;
    let mem = memory(Arc::new(MemStore::new(DEFAULT_DIM)));
    let first=PreparedEpisode::parse(&json!({"action":"append","summary":"Original scene","source":{"namespace":"graph-test","key":"first","reference":"test://episode"}})).unwrap().run(&mem,Ulid(9999),None).await.unwrap();
    let second=PreparedEpisode::parse(&json!({"action":"revise","episode_id":first["episode_id"],"expected_edition_id":first["edition_id"],"reason":"Correction","summary":"Corrected scene","source":{"namespace":"graph-test","key":"second","reference":"test://episode"}})).unwrap().run(&mem,Ulid(9999),None).await.unwrap();
    let topology = run(&mem, json!({"action":"topology"})).await;
    assert_eq!(topology["nodes"].as_array().unwrap().len(), 2);
    let cards = run(
        &mem,
        json!({"action":"summaries","ids":[first["edition_id"],second["edition_id"]]}),
    )
    .await;
    assert_eq!(cards["items"][0]["summary"], "Original scene");
    assert_eq!(cards["items"][1]["summary"], "Corrected scene");
    assert!(
        cards["items"]
            .as_array()
            .unwrap()
            .iter()
            .all(|card| card["kind"] == "episode")
    );
}

#[tokio::test]
async fn empty_graph_and_dangling_edges_are_not_invented_nodes() {
    let store = Arc::new(MemStore::new(DEFAULT_DIM));
    let mem = memory(store.clone());
    let empty = run(&mem, json!({"action":"topology","limit":1})).await;
    assert_eq!(empty["nodes"], json!([]));
    assert_eq!(empty["edges"], json!([]));
    assert_eq!(empty["has_more"], false);
    assert_eq!(empty["coverage"]["nodes_seen"], 0);
    store
        .put_edge(&Edge::new(
            NodeId(Ulid(1)),
            NodeId(Ulid(2)),
            0.7,
            EdgeKind::DerivedFrom,
            1,
        ))
        .await
        .unwrap();
    let before = store.export().canonical_value().unwrap();
    let topology = run(&mem, json!({"action":"topology","limit":1})).await;
    assert_eq!(topology["nodes"], json!([]));
    assert_eq!(topology["edges"][0]["from"], json!(Ulid(1)));
    assert_eq!(topology["edges"][0]["to"], json!(Ulid(2)));
    assert_eq!(topology["edges"][0]["kind"], "derived_from");
    assert_eq!(topology["coverage"]["edges_seen"], 1);
    let summaries = run(&mem, json!({"action":"summaries","ids":[Ulid(1),Ulid(2)]})).await;
    assert!(
        summaries["items"]
            .as_array()
            .unwrap()
            .iter()
            .all(|item| item["missing"] == true)
    );
    assert_eq!(before, store.export().canonical_value().unwrap());
}

#[test]
fn topology_tags_are_exact_raw_prefixes_with_count_and_json_byte_truncation() {
    let raw = node(
        1,
        "not in topology",
        vec!["Rust".into(), "rust-lang/rust".into(), "src/memory".into()],
    );
    let card = topology_card(&raw);
    assert_eq!(
        card["tags"],
        json!(["Rust", "rust-lang/rust", "src/memory"])
    );
    assert_eq!(card["tags_truncated"], false);
    assert!(card.get("summary").is_none() && card.get("body").is_none());
    let many = node(
        2,
        "not in topology",
        (0..64).map(|i| format!("{i:02}")).collect(),
    );
    let card = topology_card(&many);
    let prefix = card["tags"].as_array().unwrap();
    assert!(!prefix.is_empty() && prefix.len() < many.tags().count());
    assert_eq!(card["tags_truncated"], true);
    assert_eq!(
        prefix,
        &many
            .tags()
            .take(prefix.len())
            .map(|tag| json!(tag))
            .collect::<Vec<_>>()
    );
    assert!(serde_json::to_vec(&card["tags"]).unwrap().len() <= MAX_TOPOLOGY_TAGS_JSON_BYTES);
    assert_eq!(summary_card(&many)["tags_truncated"], false);
    let escaped = node(3, "not in topology", vec!["\"".repeat(127)]);
    assert_eq!(topology_card(&escaped)["tags"], json!([]));
    assert_eq!(topology_card(&escaped)["tags_truncated"], true);
    assert_eq!(summary_card(&escaped)["tags"], json!(["\"".repeat(127)]));
}

#[tokio::test]
async fn maximally_escaped_topology_pages_keep_all_nodes_and_bounded_continuations() {
    let store = Arc::new(MemStore::new(DEFAULT_DIM));
    let mem = memory(store.clone());
    let tags = (0..64)
        .map(|index| format!("{index:02}{}", "\"".repeat(124)))
        .collect::<Vec<_>>();
    for index in 1..=513 {
        store
            .put_node(&node(index, "Never exported by topology", tags.clone()))
            .await
            .unwrap();
    }
    let mut args = json!({"action":"topology","limit":256});
    let mut ids = Vec::new();
    let mut calls = 0;
    loop {
        let response = run(&mem, args.clone()).await;
        assert!(serde_json::to_vec(&response).unwrap().len() <= MAX_GRAPH_PAGE_BYTES);
        assert_eq!(response["coverage"]["body_reads"], 0);
        assert_eq!(
            response["coverage"]["tags_json_max_bytes"],
            MAX_TOPOLOGY_TAGS_JSON_BYTES
        );
        assert!(
            response["coverage"]["indexed_pages"].as_u64().unwrap()
                <= 256usize.div_ceil(MAX_MAINTENANCE_BATCH_ROWS) as u64 + 1
        );
        for card in response["nodes"].as_array().unwrap() {
            assert!(card.get("summary").is_none() && card.get("body").is_none());
            assert_eq!(card["tags"], json!([tags[0]]));
            assert_eq!(card["tags_truncated"], true);
            ids.push(card["id"].clone());
        }
        calls += 1;
        if response["next_cursor"].is_null() {
            assert_eq!(response["coverage"]["nodes_seen"], 513);
            assert_eq!(response["coverage"]["complete"], true);
            break;
        }
        assert_eq!(response["nodes"].as_array().unwrap().len(), 256);
        args["after"] = response["next_cursor"].clone();
        assert!(calls < 4);
    }
    assert_eq!(calls, 3);
    assert_eq!(ids, (1..=513).map(|id| json!(Ulid(id))).collect::<Vec<_>>());
}

#[tokio::test]
async fn eager_topology_tags_do_not_invoke_inference_or_learning() {
    use mneme_core::ports::{Embedder, Result};
    struct NoInference;
    impl Embedder for NoInference {
        fn dim(&self) -> usize {
            DEFAULT_DIM
        }
        fn fingerprint(&self) -> mneme_core::EmbeddingFingerprint {
            HashingEmbedder::new(DEFAULT_DIM).fingerprint()
        }
        fn embed<'a, 'b, 'c, 'future>(
            &'a self,
            _: &'b [&'c str],
        ) -> std::pin::Pin<
            Box<dyn std::future::Future<Output = Result<Vec<Vec<f32>>>> + Send + 'future>,
        >
        where
            'a: 'future,
            'b: 'future,
            'c: 'future,
            Self: 'future,
        {
            Box::pin(async { panic!("topology must not invoke inference") })
        }
    }
    let store = Arc::new(MemStore::new(DEFAULT_DIM));
    store
        .put_node(&node(1, "Never exported", vec!["directory:src".into()]))
        .await
        .unwrap();
    let before = store.export().canonical_value().unwrap();
    let mem = Memory::new(
        store.clone(),
        store.clone(),
        store.clone(),
        Arc::new(NoInference),
        Arc::new(SystemClock),
        Config::default(),
    )
    .with_body_store(Arc::new(mneme_body::InlineStore::new()));
    let page = run(&mem, json!({"action":"topology"})).await;
    assert_eq!(page["nodes"][0]["tags"], json!(["directory:src"]));
    assert_eq!(page["coverage"]["body_reads"], 0);
    assert_eq!(before, store.export().canonical_value().unwrap());
}
